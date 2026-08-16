//! Response capacity is bounded independently by bytes and items.

use aster_transport_core::framing::MAX_FRAME_SIZE;
use aster_transport_core::reactor::{
    response_channel, response_channel_with_limits, OutgoingFrame, ResponseReserveError,
    FRAME_CHANNEL_DEPTH, RESPONSE_BYTE_LIMIT, RESPONSE_ITEM_LIMIT,
};

#[tokio::test]
async fn byte_budget_includes_the_writer_in_flight_item() {
    let (tx, mut rx) = response_channel_with_limits(16, 1024);
    tx.try_reserve(600)
        .expect("first charge fits")
        .send(OutgoingFrame::Frame(vec![0; 600]))
        .expect("length matches reservation");

    let in_flight = rx.recv().await.expect("writer receives the item");
    assert_eq!(
        tx.try_reserve(425).err(),
        Some(ResponseReserveError::Full),
        "receiving must not release the byte charge before the write completes",
    );
    assert!(
        tx.try_reserve(424).is_ok(),
        "the exact remaining byte budget is usable",
    );

    drop(in_flight);
    assert!(
        tx.try_reserve(1024).is_ok(),
        "finishing the write releases its byte charge",
    );
}

#[tokio::test]
async fn item_limit_includes_the_writer_in_flight_item() {
    let item_limit = 8;
    let (tx, mut rx) = response_channel_with_limits(item_limit, 1024);
    for _ in 0..item_limit {
        tx.try_reserve(0)
            .expect("item slot fits")
            .send(OutgoingFrame::Frame(Vec::new()))
            .expect("zero-byte charge matches");
    }

    let in_flight = rx.recv().await.expect("writer receives one item");
    assert_eq!(
        tx.try_reserve(0).err(),
        Some(ResponseReserveError::Full),
        "receiving must not release the item slot before the write completes",
    );
    drop(in_flight);
    assert!(tx.try_reserve(0).is_ok());
}

#[tokio::test]
async fn one_hundred_thousand_zero_byte_frames_stop_at_the_item_ceiling() {
    let (tx, _rx) = response_channel();
    let mut accepted = 0;
    for _ in 0..100_000 {
        match tx.try_reserve(0) {
            Ok(permit) => {
                permit
                    .send(OutgoingFrame::Frame(Vec::new()))
                    .expect("zero-byte charge matches");
                accepted += 1;
            }
            Err(ResponseReserveError::Full) => {}
            Err(error) => panic!("unexpected reservation result: {error:?}"),
        }
    }
    assert_eq!(accepted, RESPONSE_ITEM_LIMIT);
}

#[tokio::test]
async fn oversized_item_is_refused_without_consuming_an_item_slot() {
    let (tx, _rx) = response_channel_with_limits(1, 16);
    assert_eq!(
        tx.try_reserve(17).err(),
        Some(ResponseReserveError::TooLarge),
    );
    assert!(
        tx.try_reserve(16).is_ok(),
        "the failed byte reservation must release its item permit",
    );
}

#[tokio::test]
async fn closed_is_distinguishable_from_full() {
    let (tx, rx) = response_channel_with_limits(1, 16);
    drop(rx);
    assert_eq!(tx.try_reserve(1).err(), Some(ResponseReserveError::Closed),);
}

const _: () = assert!(RESPONSE_BYTE_LIMIT == FRAME_CHANNEL_DEPTH * MAX_FRAME_SIZE as usize);
const _: () = assert!(RESPONSE_ITEM_LIMIT >= 100);
