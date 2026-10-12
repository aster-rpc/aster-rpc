//! Async service adapter for the fixed privileged local helper.
use crate::{
    authorization::AuthorizedSubmit,
    contract::*,
    linux_helper::{self, HelperRequestV1 as Q, HelperResponseV1 as R},
    service::{ExecRuntime, error},
};
use async_trait::async_trait;
use std::path::PathBuf;
pub struct LinuxRuntime {
    pub socket: PathBuf,
    pub target_capabilities: TargetCapabilitiesV1,
    pub guarantees: GuaranteesV1,
}
impl LinuxRuntime {
    async fn call(&self, q: Q) -> Result<R, ExecErrorV1> {
        let socket = self.socket.clone();
        let response = tokio::task::spawn_blocking(move || linux_helper::call(&socket, &q))
            .await
            .map_err(|_| {
                error(
                    ExecErrorCodeV1::SupervisorUnavailable,
                    "helper worker unavailable",
                )
            })?
            .map_err(|_| {
                error(
                    ExecErrorCodeV1::SupervisorUnavailable,
                    "Exec helper unavailable",
                )
            })?;
        match response {
            R::Error(error) => Err(error),
            response => Ok(response),
        }
    }
    fn unexpected() -> ExecErrorV1 {
        error(
            ExecErrorCodeV1::SupervisorUnavailable,
            "unexpected helper response",
        )
    }
}
#[async_trait]
impl ExecRuntime for LinuxRuntime {
    async fn capabilities(&self) -> Result<TargetCapabilitiesV1, ExecErrorV1> {
        let mut caps = self.target_capabilities.clone();
        caps.backend_ready = self.call(Q::Ping).await.is_ok();
        if !caps.backend_ready {
            caps.unavailable_reason = Some("privileged helper unavailable".into());
        }
        Ok(caps)
    }
    async fn launch(
        &self,
        request: ExecRequestV1,
        authorized: AuthorizedSubmit,
        job_id: String,
        digest: [u8; 32],
        caller: String,
        accepted_at_ms: u64,
    ) -> Result<JobStatusV1, ExecErrorV1> {
        if std::path::Path::new(&request.cwd) != authorized.canonical_cwd {
            return Err(ExecErrorV1::invalid("working directory must be canonical"));
        }
        let mut guarantees = self.guarantees.clone();
        guarantees.profile = authorized.profile;
        let status = JobStatusV1 {
            job: JobRefV1 {
                job_id,
                target_id: request.target_id.clone(),
            },
            state: JobStateV1::Accepted,
            caller_id: caller,
            resolved_target: request.target_id.clone(),
            execution_identity: authorized.account_alias,
            accepted_at_ms,
            started_at_ms: None,
            finished_at_ms: None,
            exit_code: None,
            signal: None,
            reason_code: None,
            output_cursor: 0,
            guarantees,
        };
        match self
            .call(Q::Launch {
                request: Box::new(request),
                status: Box::new(status),
                request_digest: digest,
            })
            .await?
        {
            R::Status(s) => Ok(*s),
            _ => Err(Self::unexpected()),
        }
    }
    async fn status(&self, job: &JobRefV1) -> Result<JobStatusV1, ExecErrorV1> {
        match self
            .call(Q::Status {
                job_id: job.job_id.clone(),
            })
            .await?
        {
            R::Status(s) if s.job == *job => Ok(*s),
            _ => Err(Self::unexpected()),
        }
    }
    async fn output(&self, r: &OutputRequestV1) -> Result<OutputPageV1, ExecErrorV1> {
        match self
            .call(Q::Output {
                job_id: r.job.job_id.clone(),
                cursor: r.cursor,
                max_bytes: r.max_bytes,
            })
            .await?
        {
            R::Output(p) => Ok(p),
            _ => Err(Self::unexpected()),
        }
    }
    async fn cancel(&self, r: &CancelRequestV1) -> Result<CancelReceiptV1, ExecErrorV1> {
        match self
            .call(Q::Cancel {
                job_id: r.job.job_id.clone(),
                grace_ms: r.grace_ms,
            })
            .await?
        {
            R::Cancel(p) => Ok(p),
            _ => Err(Self::unexpected()),
        }
    }
    async fn list_jobs(&self) -> Result<Vec<JobStatusV1>, ExecErrorV1> {
        match self.call(Q::Enumerate).await? {
            R::Jobs(j) => Ok(j),
            _ => Err(Self::unexpected()),
        }
    }
    async fn pending_outcomes(&self) -> Result<Vec<(String, Vec<u8>)>, ExecErrorV1> {
        let mut result = Vec::new();
        let jobs = match self.call(Q::EnumerateOutcomes).await? {
            R::Jobs(jobs) => jobs,
            _ => return Err(Self::unexpected()),
        };
        for status in jobs {
            if !status.state.is_terminal() {
                continue;
            }
            let job_id = status.job.job_id;
            match self
                .call(Q::Outcome {
                    job_id: job_id.clone(),
                })
                .await
            {
                Ok(R::Outcome(Some(bytes))) => result.push((job_id, bytes)),
                Ok(R::Outcome(None)) => {}
                _ => eprintln!("pending outcome unavailable for {job_id}"),
            }
        }
        Ok(result)
    }
    async fn ack_outcome(&self, job_id: &str) -> Result<(), ExecErrorV1> {
        match self
            .call(Q::AckOutcome {
                job_id: job_id.into(),
            })
            .await?
        {
            R::Ack => Ok(()),
            _ => Err(Self::unexpected()),
        }
    }
}
