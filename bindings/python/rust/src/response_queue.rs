//! Bounded response read-ahead shared by Python server-stream and bidi handles.
use std::future::Future;
use std::sync::Arc;

use aster::rpc::ResponseStream;
use tokio::sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore};

pub(crate) const READ_AHEAD: usize = 64;
pub(crate) const RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) trait ResponseSource: Send + 'static {
    fn next(&mut self) -> impl Future<Output = Option<aster::Result<Vec<u8>>>> + Send;
}

impl ResponseSource for ResponseStream {
    fn next(&mut self) -> impl Future<Output = Option<aster::Result<Vec<u8>>>> + Send {
        ResponseStream::next(self)
    }
}

enum StreamItem {
    Item(Vec<u8>, OwnedSemaphorePermit),
    Error(aster::Error),
}

struct Reader {
    rx: mpsc::Receiver<StreamItem>,
    pending_error: Option<aster::Error>,
}

pub(crate) struct ResponseReadAhead {
    reader: Mutex<Reader>,
    pump: tokio::task::JoinHandle<()>,
    #[cfg(test)]
    budget: Arc<Semaphore>,
}

impl ResponseReadAhead {
    pub(crate) fn spawn(mut source: impl ResponseSource) -> Self {
        let (tx, rx) = mpsc::channel(READ_AHEAD);
        let budget = Arc::new(Semaphore::new(RESPONSE_BYTES));
        let pump_budget = budget.clone();
        let pump = tokio::spawn(async move {
            loop {
                // Count the fetched frame as in flight, not just channel items.
                let Ok(slot) = tx.reserve().await else {
                    return;
                };
                // Reserve before reading: a queued oversized frame owns the full
                // budget and prevents another large in-flight allocation.
                let Ok(mut permit) = pump_budget.clone().acquire_owned().await else {
                    return;
                };
                let item = match source.next().await {
                    None => return,
                    Some(Ok(payload)) => {
                        // A legal frame larger than the queue budget is admitted
                        // only when the queue is empty; never reject it for size.
                        let charge = payload.len().clamp(1, RESPONSE_BYTES);
                        if charge > 1 {
                            let Ok(rest) = pump_budget
                                .clone()
                                .acquire_many_owned((charge - 1) as u32)
                                .await
                            else {
                                return;
                            };
                            permit.merge(rest);
                        }
                        StreamItem::Item(payload, permit)
                    }
                    Some(Err(error)) => {
                        drop(permit);
                        StreamItem::Error(error)
                    }
                };
                let last = matches!(item, StreamItem::Error(_));
                slot.send(item);
                if last {
                    return;
                }
            }
        });
        Self {
            reader: Mutex::new(Reader {
                rx,
                pending_error: None,
            }),
            pump,
            #[cfg(test)]
            budget,
        }
    }

    pub(crate) async fn next_batch(&self, max: usize) -> aster::Result<Vec<Vec<u8>>> {
        let mut reader = self.reader.lock().await;
        if let Some(error) = reader.pending_error.take() {
            return Err(error);
        }
        let max = max.clamp(1, READ_AHEAD);
        let mut out = Vec::new();
        let mut bytes = 0;
        match reader.rx.recv().await {
            None => return Ok(out),
            Some(StreamItem::Error(error)) => return Err(error),
            Some(StreamItem::Item(payload, permit)) => {
                bytes += payload.len();
                out.push(payload);
                drop(permit);
            }
        }
        // One legal frame may cross the byte threshold. There is no arbitrary
        // max-sized batch while another thread continually refills the channel.
        while out.len() < max && bytes < RESPONSE_BYTES {
            match reader.rx.try_recv() {
                Ok(StreamItem::Item(payload, permit)) => {
                    bytes += payload.len();
                    out.push(payload);
                    drop(permit);
                }
                Ok(StreamItem::Error(error)) => {
                    reader.pending_error = Some(error);
                    break;
                }
                Err(_) => break,
            }
        }
        Ok(out)
    }

    pub(crate) fn cancel(&self) {
        self.pump.abort();
    }
}

impl Drop for ResponseReadAhead {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::future::ready;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;
    use tokio::time::{timeout, Duration};

    struct Source {
        frames: VecDeque<aster::Result<Vec<u8>>>,
        reads: Arc<AtomicUsize>,
        entered: Arc<Notify>,
        signal_at: usize,
    }

    impl ResponseSource for Source {
        fn next(&mut self) -> impl Future<Output = Option<aster::Result<Vec<u8>>>> + Send {
            let reads = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
            if reads == self.signal_at {
                self.entered.notify_one();
            }
            ready(self.frames.pop_front())
        }
    }

    fn source(
        frames: Vec<aster::Result<Vec<u8>>>,
        signal_at: usize,
    ) -> (Source, Arc<Notify>, Arc<AtomicUsize>) {
        let entered = Arc::new(Notify::new());
        let reads = Arc::new(AtomicUsize::new(0));
        (
            Source {
                frames: frames.into(),
                reads: reads.clone(),
                entered: entered.clone(),
                signal_at,
            },
            entered,
            reads,
        )
    }

    #[tokio::test]
    async fn byte_budget_stops_read_ahead_and_abort_releases_in_flight_permit() {
        let size = 5 * 1024 * 1024;
        let (source, entered, _) = source((0..3).map(|_| Ok(vec![7; size])).collect(), 2);
        let mut queue = ResponseReadAhead::spawn(source);
        timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        // Current-thread source notifications resume us after the pump parks:
        // exactly one frame is queued; a second owns the one-byte read permit.
        let queued = queue.reader.lock().await.rx.len();
        assert!(
            queued * size <= RESPONSE_BYTES,
            "queued {queued} frames / {} bytes",
            queued * size
        );
        assert_eq!(queued, 1);
        let before = queue.budget.available_permits();
        queue.cancel();
        assert!(timeout(Duration::from_secs(2), &mut queue.pump)
            .await
            .unwrap()
            .unwrap_err()
            .is_cancelled());
        assert!(before < RESPONSE_BYTES - size);
        assert_eq!(queue.budget.available_permits(), RESPONSE_BYTES - size);
        assert_eq!(queue.next_batch(64).await.unwrap()[0].len(), size);
        assert_eq!(queue.budget.available_permits(), RESPONSE_BYTES);
        assert!(queue.next_batch(64).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn legal_oversized_frame_is_admitted_alone_and_delivered() {
        let size = 16 * 1024 * 1024;
        let (source, entered, reads) = source(vec![Ok(vec![1; size]), Ok(vec![2; size])], 1);
        let queue = ResponseReadAhead::spawn(source);
        timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert_eq!(queue.reader.lock().await.rx.len(), 1);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "read another large frame while queue held one"
        );
        for value in [1, 2] {
            let batch = timeout(Duration::from_secs(2), queue.next_batch(usize::MAX))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(batch.len(), 1);
            assert_eq!(batch[0].len(), size);
            assert!(batch[0].iter().all(|byte| *byte == value));
        }
        assert!(queue.next_batch(64).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn huge_requested_batch_is_capped_even_for_zero_byte_frames() {
        let (source, entered, reads) = source((0..1000).map(|_| Ok(Vec::new())).collect(), 2);
        let queue = ResponseReadAhead::spawn(source);
        timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert_eq!(
            reads.load(Ordering::SeqCst),
            READ_AHEAD,
            "pump read beyond its item slots"
        );
        let batch = queue.next_batch(usize::MAX).await.unwrap();
        assert_eq!(batch.len(), READ_AHEAD);
        assert!(batch.iter().all(Vec::is_empty));
        queue.cancel();
    }

    #[tokio::test]
    async fn batch_stops_at_byte_threshold() {
        let size = 1024 * 1024;
        let (source, entered, _) = source((0..20).map(|_| Ok(vec![3; size])).collect(), 8);
        let queue = ResponseReadAhead::spawn(source);
        timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        let batch = queue.next_batch(usize::MAX).await.unwrap();
        assert_eq!(batch.len(), 8);
        assert_eq!(batch.iter().map(Vec::len).sum::<usize>(), RESPONSE_BYTES);
        queue.cancel();
    }

    #[tokio::test]
    async fn error_is_returned_after_preceding_payloads() {
        let (source, entered, _) = source(
            vec![
                Ok(vec![1]),
                Err(aster::Error::InvalidArgument("terminal".into())),
            ],
            2,
        );
        let queue = ResponseReadAhead::spawn(source);
        timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert_eq!(queue.next_batch(64).await.unwrap(), vec![vec![1]]);
        assert!(
            matches!(queue.next_batch(64).await, Err(aster::Error::InvalidArgument(message)) if message == "terminal")
        );
        assert!(queue.next_batch(64).await.unwrap().is_empty());
    }
}
