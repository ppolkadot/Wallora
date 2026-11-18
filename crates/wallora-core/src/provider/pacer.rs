use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, oneshot};
use tokio::time::Instant;

use crate::domain::RequestPriority;
use crate::provider::api::MAX_COOLDOWN;
use crate::provider::error::ProviderError;

pub const MIN_SPACING: Duration = Duration::from_millis(1500);
pub const WINDOW_DURATION: Duration = Duration::from_secs(60);
pub const WINDOW_CAP: usize = 40;
pub const MAX_IN_FLIGHT: usize = 2;
pub const INTERACTIVE_CAP: usize = 16;
pub const VISIBLE_CAP: usize = 64;
pub const PREFETCH_CAP: usize = 64;
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);
pub const MAX_RETRIES: usize = 2;
/// Backoff applied before retry 1 and retry 2, measured from the preceding failure.
pub const TRANSIENT_BACKOFF: [Duration; MAX_RETRIES] =
    [Duration::from_secs(2), Duration::from_secs(4)];

#[derive(Debug)]
pub enum AttemptOutcome {
    Success(Vec<u8>),
    RetryableRateLimit { retry_after: Option<Duration> },
    RetryableTransient(ProviderError),
    Terminal(ProviderError),
}

type JobResult = Result<Vec<u8>, ProviderError>;
type SubscriberReceiver = oneshot::Receiver<JobResult>;

struct Admission {
    job_id: u64,
    sub_id: u64,
    rx: SubscriberReceiver,
}

/// Lifecycle position of an admitted job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Queued,
    Running,
    WaitingRetry,
}

struct Subscriber {
    id: u64,
    tx: oneshot::Sender<JobResult>,
}

struct Job {
    id: u64,
    key: String,
    /// Effective priority. Promotion only ever raises it and stays sticky until retirement.
    priority: RequestPriority,
    phase: Phase,
    subscribers: Vec<Subscriber>,
}

impl Job {
    fn new(id: u64, key: String, priority: RequestPriority, phase: Phase) -> Self {
        Self {
            id,
            key,
            priority,
            phase,
            subscribers: Vec::new(),
        }
    }
}

fn priority_rank(priority: RequestPriority) -> u8 {
    match priority {
        RequestPriority::Interactive => 0,
        RequestPriority::Visible => 1,
        RequestPriority::Prefetch => 2,
    }
}

fn retry_backoff(retry_index: usize) -> Duration {
    TRANSIENT_BACKOFF
        .get(retry_index.saturating_sub(1))
        .copied()
        .unwrap_or_else(|| TRANSIENT_BACKOFF.last().copied().unwrap_or(Duration::ZERO))
}

#[derive(Default)]
struct PacerState {
    next_job_id: u64,
    next_subscriber_id: u64,
    jobs: HashMap<u64, Job>,
    interactive: VecDeque<u64>,
    visible: VecDeque<u64>,
    prefetch: VecDeque<u64>,
    in_flight: usize,
    last_start: Option<Instant>,
    start_log: VecDeque<Instant>,
    cooldown_until: Option<Instant>,
}

impl PacerState {
    fn queue_mut(&mut self, priority: RequestPriority) -> &mut VecDeque<u64> {
        match priority {
            RequestPriority::Interactive => &mut self.interactive,
            RequestPriority::Visible => &mut self.visible,
            RequestPriority::Prefetch => &mut self.prefetch,
        }
    }

    fn remove_from_queues(&mut self, id: u64) {
        for queue in [&mut self.interactive, &mut self.visible, &mut self.prefetch] {
            if let Some(pos) = queue.iter().position(|queued| *queued == id) {
                queue.remove(pos);
                return;
            }
        }
    }

    /// Removes a job and everything it owns, returning it once. Idempotent by job id.
    fn retire(&mut self, id: u64) -> Option<Job> {
        self.remove_from_queues(id);
        self.jobs.remove(&id)
    }

    /// Searches the bounded queues for a queued job with the same coalescing key.
    fn find_queued_by_key(&self, key: &str) -> Option<u64> {
        for queue in [&self.interactive, &self.visible, &self.prefetch] {
            for id in queue {
                if self.jobs.get(id).is_some_and(|job| job.key == key) {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// Highest-priority queue front; strict priority order across the three queues.
    fn next_eligible_id(&self) -> Option<u64> {
        self.interactive
            .front()
            .or_else(|| self.visible.front())
            .or_else(|| self.prefetch.front())
            .copied()
    }

    fn prune_window(&mut self, now: Instant) {
        while let Some(&oldest) = self.start_log.front() {
            if now.saturating_duration_since(oldest) > WINDOW_DURATION {
                self.start_log.pop_front();
            } else {
                break;
            }
        }
    }

    fn earliest_attempt_start(&mut self, now: Instant) -> Instant {
        self.prune_window(now);
        let mut earliest = now;
        if let Some(last) = self.last_start {
            earliest = earliest.max(last + MIN_SPACING);
        }
        if self.start_log.len() >= WINDOW_CAP
            && let Some(&oldest) = self.start_log.front()
        {
            // Strict rolling 60s window: wait just past 60s from the oldest start still in
            // the window so no inclusive 60s span ever contains more than 40 starts.
            earliest = earliest.max(oldest + WINDOW_DURATION + Duration::from_millis(1));
        }
        if let Some(cooldown) = self.cooldown_until {
            earliest = earliest.max(cooldown);
        }
        earliest
    }

    fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
        self.cooldown_until
            .filter(|until| *until > now)
            .map(|until| until.saturating_duration_since(now))
    }

    fn next_subscriber(&mut self) -> (u64, oneshot::Sender<JobResult>, SubscriberReceiver) {
        let sub_id = self.next_subscriber_id;
        self.next_subscriber_id = self.next_subscriber_id.wrapping_add(1);
        let (tx, rx) = oneshot::channel();
        (sub_id, tx, rx)
    }

    /// Admits a brand-new job with no subscribers yet, applying queue overflow rules.
    fn admit_new(&mut self, key: String, priority: RequestPriority) -> Result<u64, ProviderError> {
        if priority == RequestPriority::Interactive && self.interactive.len() >= INTERACTIVE_CAP {
            return Err(ProviderError::Busy);
        }

        let mut evicted = Vec::new();
        match priority {
            RequestPriority::Interactive => {}
            RequestPriority::Visible => {
                while self.visible.len() >= VISIBLE_CAP {
                    let Some(oldest) = self.visible.pop_front() else {
                        break;
                    };
                    if let Some(job) = self.jobs.remove(&oldest) {
                        evicted.push(job);
                    }
                }
            }
            RequestPriority::Prefetch => {
                while self.prefetch.len() >= PREFETCH_CAP {
                    let Some(oldest) = self.prefetch.pop_front() else {
                        break;
                    };
                    if let Some(job) = self.jobs.remove(&oldest) {
                        evicted.push(job);
                    }
                }
            }
        }

        let id = self.next_job_id;
        self.next_job_id = self.next_job_id.wrapping_add(1);
        self.jobs
            .insert(id, Job::new(id, key, priority, Phase::Queued));
        self.queue_mut(priority).push_back(id);

        for job in evicted {
            resolve(job.subscribers, Err(ProviderError::Busy));
        }
        Ok(id)
    }

    /// Registers one caller as a subscriber of `id`.
    fn push_subscriber(&mut self, id: u64) -> Result<(u64, SubscriberReceiver), ProviderError> {
        let (sub_id, tx, rx) = self.next_subscriber();
        let job = self.jobs.get_mut(&id).ok_or(ProviderError::Busy)?;
        job.subscribers.push(Subscriber { id: sub_id, tx });
        Ok((sub_id, rx))
    }

    /// Raises a queued job's effective priority, honouring destination queue capacity.
    fn promote_queued(&mut self, id: u64, priority: RequestPriority) -> Result<(), ProviderError> {
        if priority == RequestPriority::Interactive && self.interactive.len() >= INTERACTIVE_CAP {
            return Err(ProviderError::Busy);
        }

        let mut evicted = Vec::new();
        if priority == RequestPriority::Visible {
            while self.visible.len() >= VISIBLE_CAP {
                let Some(oldest) = self.visible.pop_front() else {
                    break;
                };
                if let Some(job) = self.jobs.remove(&oldest) {
                    evicted.push(job);
                }
            }
        }

        self.remove_from_queues(id);
        if let Some(job) = self.jobs.get_mut(&id) {
            job.priority = priority;
        }
        self.queue_mut(priority).push_back(id);

        for job in evicted {
            resolve(job.subscribers, Err(ProviderError::Busy));
        }
        Ok(())
    }

    /// Cancels queued Prefetch work and any Prefetch waiting to retry.
    fn drain_prefetch_for_cooldown(&mut self, remaining: Duration) {
        let error = ProviderError::RateLimited {
            retry_after: Some(remaining),
        };
        let mut targets: Vec<u64> = self.prefetch.iter().copied().collect();
        targets.extend(
            self.jobs
                .values()
                .filter(|job| {
                    job.priority == RequestPriority::Prefetch && job.phase == Phase::WaitingRetry
                })
                .map(|job| job.id),
        );
        for id in targets {
            if let Some(job) = self.retire(id) {
                resolve(job.subscribers, Err(error.clone()));
            }
        }
    }
}

fn resolve(subscribers: Vec<Subscriber>, result: JobResult) {
    for subscriber in subscribers {
        let _ = subscriber.tx.send(result.clone());
    }
}

struct PacerShared {
    state: Mutex<PacerState>,
    notify: Notify,
}

impl Default for PacerShared {
    fn default() -> Self {
        Self {
            state: Mutex::new(PacerState::default()),
            notify: Notify::new(),
        }
    }
}

impl PacerShared {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, PacerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn is_active(&self, job_id: u64) -> bool {
        self.lock_state().jobs.contains_key(&job_id)
    }
}

/// Holds one attempt permit. It is released when the attempt future is dropped or returns,
/// and never across backoff.
struct AttemptPermit {
    shared: Arc<PacerShared>,
}

impl Drop for AttemptPermit {
    fn drop(&mut self) {
        {
            let mut state = self.shared.lock_state();
            state.in_flight = state.in_flight.saturating_sub(1);
        }
        self.shared.notify.notify_waiters();
    }
}

/// Unregisters one caller. When the last caller leaves, the job is retired immediately.
struct SubscriberGuard {
    shared: Arc<PacerShared>,
    job_id: u64,
    sub_id: u64,
}

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        let retired = {
            let mut state = self.shared.lock_state();
            let Some(job) = state.jobs.get_mut(&self.job_id) else {
                return;
            };
            if let Some(pos) = job.subscribers.iter().position(|sub| sub.id == self.sub_id) {
                job.subscribers.remove(pos);
            }
            if !job.subscribers.is_empty() {
                return;
            }
            state.retire(self.job_id).is_some()
        };
        if retired {
            self.shared.notify.notify_waiters();
        }
    }
}

/// Retires the worker's job on unwind so a panicking worker cannot wedge a queue front.
struct WorkerGuard {
    shared: Arc<PacerShared>,
    job_id: u64,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let retired = {
            let mut state = self.shared.lock_state();
            state.retire(self.job_id).is_some()
        };
        if retired {
            self.shared.notify.notify_waiters();
        }
    }
}

/// Tokenless sliding-log pacer + three-tier priority queues + coalescing for Wallhaven API calls.
#[derive(Clone, Default)]
pub struct ApiPacer {
    shared: Arc<PacerShared>,
}

impl ApiPacer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Admits a job (or joins an identical queued one), subscribes the caller, and awaits the
    /// single shared result. The first caller behaves exactly like every other subscriber.
    pub async fn run<F, Fut>(
        &self,
        coalesce_key: String,
        priority: RequestPriority,
        attempt_fn: F,
    ) -> JobResult
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Result<AttemptOutcome, ProviderError>> + Send + 'static,
    {
        let Admission { job_id, sub_id, rx } = self.admit(coalesce_key, priority, attempt_fn)?;
        let guard = SubscriberGuard {
            shared: Arc::clone(&self.shared),
            job_id,
            sub_id,
        };
        let result = rx.await.unwrap_or(Err(ProviderError::Busy));
        drop(guard);
        result
    }

    fn admit<F, Fut>(
        &self,
        coalesce_key: String,
        priority: RequestPriority,
        attempt_fn: F,
    ) -> Result<Admission, ProviderError>
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Result<AttemptOutcome, ProviderError>> + Send + 'static,
    {
        let start_worker;
        let job_id;
        let sub_id;
        let rx;
        {
            let mut state = self.shared.lock_state();

            // A prefetch submission during active cooldown is rejected before coalescing.
            if priority == RequestPriority::Prefetch
                && let Some(remaining) = state.cooldown_remaining(Instant::now())
            {
                return Err(ProviderError::RateLimited {
                    retry_after: Some(remaining),
                });
            }

            match state.find_queued_by_key(&coalesce_key) {
                Some(existing) => {
                    start_worker = false;
                    let current = state
                        .jobs
                        .get(&existing)
                        .map(|job| job.priority)
                        .unwrap_or(priority);
                    if priority_rank(priority) < priority_rank(current) {
                        state.promote_queued(existing, priority)?;
                    }
                    job_id = existing;
                }
                None => {
                    start_worker = true;
                    job_id = state.admit_new(coalesce_key, priority)?;
                }
            }

            let (id, receiver) = state.push_subscriber(job_id)?;
            sub_id = id;
            rx = receiver;
        }

        if start_worker {
            let shared = Arc::clone(&self.shared);
            let span = tracing::Span::current();
            tokio::spawn(tracing::Instrument::instrument(
                run_worker(shared, job_id, attempt_fn),
                span,
            ));
        }

        // Admitted work, promotion and eviction can all unblock a queue front.
        self.shared.notify.notify_waiters();
        Ok(Admission { job_id, sub_id, rx })
    }
}

fn cooldown_deadline(now: Instant, duration: Duration) -> Instant {
    now.checked_add(duration)
        .or_else(|| now.checked_add(DEFAULT_COOLDOWN))
        .unwrap_or(now)
}

/// Applies a 429 cooldown under the scheduler lock before the attempt slot is released.
fn activate_cooldown(shared: &PacerShared, retry_after: Option<Duration>) -> Duration {
    let requested = retry_after.unwrap_or(DEFAULT_COOLDOWN).min(MAX_COOLDOWN);
    let now = Instant::now();
    let remaining = {
        let mut state = shared.lock_state();
        let proposed = cooldown_deadline(now, requested);
        let deadline = state
            .cooldown_until
            .map_or(proposed, |current| current.max(proposed));
        state.cooldown_until = Some(deadline);
        let remaining = deadline.saturating_duration_since(now);
        state.drain_prefetch_for_cooldown(remaining);
        remaining
    };
    shared.notify.notify_waiters();
    remaining
}

async fn wait_until_retired(shared: &Arc<PacerShared>, job_id: u64) {
    loop {
        let notified = shared.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !shared.is_active(job_id) {
            return;
        }
        notified.await;
    }
}

async fn wait_for_slot(
    shared: &Arc<PacerShared>,
    job_id: u64,
    initial: bool,
    not_before: Option<Instant>,
) -> Result<AttemptPermit, ()> {
    loop {
        let notified = shared.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let now = Instant::now();
        let mut started = false;
        let mut wake_at: Option<Instant> = None;
        {
            let mut state = shared.lock_state();
            if !state.jobs.contains_key(&job_id) {
                return Err(());
            }
            let at_front = state.next_eligible_id() == Some(job_id);
            if (!initial || at_front) && state.in_flight < MAX_IN_FLIGHT {
                let earliest = state.earliest_attempt_start(now);
                let ready_at = not_before.map_or(earliest, |deadline| earliest.max(deadline));
                if now >= ready_at {
                    if initial {
                        state.remove_from_queues(job_id);
                    }
                    if let Some(job) = state.jobs.get_mut(&job_id) {
                        job.phase = Phase::Running;
                    }
                    state.in_flight += 1;
                    state.last_start = Some(now);
                    state.start_log.push_back(now);
                    started = true;
                } else {
                    wake_at = Some(ready_at);
                }
            } else {
                wake_at = not_before.filter(|deadline| *deadline > now);
            }
        }

        if started {
            // Notify other waiters only after releasing the lock.
            shared.notify.notify_waiters();
            return Ok(AttemptPermit {
                shared: Arc::clone(shared),
            });
        }

        if let Some(target) = wake_at {
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(target) => {}
            }
        } else {
            notified.await;
        }
    }
}

fn mark_running_retry(shared: &PacerShared, job_id: u64) -> bool {
    let mut state = shared.lock_state();
    match state.jobs.get_mut(&job_id) {
        Some(job) => {
            job.phase = Phase::WaitingRetry;
            true
        }
        None => false,
    }
}

fn job_priority(shared: &PacerShared, job_id: u64) -> Option<RequestPriority> {
    shared
        .lock_state()
        .jobs
        .get(&job_id)
        .map(|job| job.priority)
}

async fn run_worker<F, Fut>(shared: Arc<PacerShared>, job_id: u64, attempt_fn: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Result<AttemptOutcome, ProviderError>> + Send + 'static,
{
    let guard = WorkerGuard {
        shared: Arc::clone(&shared),
        job_id,
    };

    let mut retry_index = 0usize;
    let mut not_before: Option<Instant> = None;

    let result = loop {
        let initial = retry_index == 0;
        let permit = match wait_for_slot(&shared, job_id, initial, not_before).await {
            Ok(permit) => permit,
            Err(()) => break Err(ProviderError::Busy),
        };

        let attempt = tokio::select! {
            biased;
            () = wait_until_retired(&shared, job_id) => None,
            outcome = attempt_fn() => Some(outcome),
        };

        let Some(outcome) = attempt else {
            // The job was retired while the attempt was in flight; drop it and stay silent.
            drop(permit);
            break Err(ProviderError::Busy);
        };

        match outcome {
            Ok(AttemptOutcome::Success(bytes)) => {
                drop(permit);
                break Ok(bytes);
            }
            Ok(AttemptOutcome::Terminal(err)) | Err(err) => {
                drop(permit);
                break Err(err);
            }
            Ok(AttemptOutcome::RetryableRateLimit { retry_after }) => {
                // Apply the cooldown before the slot becomes available, including the final try.
                let remaining = activate_cooldown(&shared, retry_after);
                let effective = job_priority(&shared, job_id);
                drop(permit);
                if effective == Some(RequestPriority::Prefetch) || retry_index >= MAX_RETRIES {
                    break Err(ProviderError::RateLimited {
                        retry_after: Some(remaining),
                    });
                }
                retry_index += 1;
                if !mark_running_retry(&shared, job_id) {
                    break Err(ProviderError::Busy);
                }
                not_before = Some(Instant::now() + retry_backoff(retry_index));
            }
            Ok(AttemptOutcome::RetryableTransient(err)) => {
                drop(permit);
                if retry_index >= MAX_RETRIES {
                    break Err(err);
                }
                retry_index += 1;
                if !mark_running_retry(&shared, job_id) {
                    break Err(ProviderError::Busy);
                }
                not_before = Some(Instant::now() + retry_backoff(retry_index));
            }
        }
    };

    let job = {
        let mut state = shared.lock_state();
        state.retire(job_id)
    };
    if let Some(job) = job {
        resolve(job.subscribers, result);
    }
    drop(guard);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{Mutex, mpsc};
    use tokio::time::{Duration, Instant};

    type StartSender = mpsc::UnboundedSender<Instant>;

    fn starts_channel() -> (StartSender, mpsc::UnboundedReceiver<Instant>) {
        mpsc::unbounded_channel()
    }

    /// Collects every start already recorded, then stops at the first idle second.
    async fn drain_starts(rx: &mut mpsc::UnboundedReceiver<Instant>) -> Vec<Instant> {
        let mut seen = Vec::new();
        while let Ok(Some(at)) = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            seen.push(at);
        }
        seen
    }

    #[tokio::test(start_paused = true)]
    async fn enforces_minimum_spacing_of_1500ms_between_starts() {
        let pacer = Arc::new(ApiPacer::new());
        let starts = Arc::new(Mutex::new(Vec::<Instant>::new()));

        let mut handles = Vec::new();
        for i in 0..5 {
            let p = Arc::clone(&pacer);
            let s = Arc::clone(&starts);
            handles.push(tokio::spawn(async move {
                p.run(
                    format!("req-{i}"),
                    RequestPriority::Interactive,
                    move || {
                        let s = Arc::clone(&s);
                        async move {
                            s.lock().await.push(Instant::now());
                            Ok(AttemptOutcome::Success(vec![1]))
                        }
                    },
                )
                .await
                .unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let log = starts.lock().await;
        assert_eq!(log.len(), 5);
        for pair in log.windows(2) {
            assert!(
                pair[1].duration_since(pair[0]) >= Duration::from_millis(1500),
                "two starts occurred less than 1.5s apart: {:?}",
                pair[1].duration_since(pair[0])
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn enforces_rolling_60s_cap_under_200_request_burst_with_retries() {
        let pacer = Arc::new(ApiPacer::new());
        let starts = Arc::new(Mutex::new(Vec::<Instant>::new()));

        // Run 4 contiguous batches of 50 Visible requests (200 total requests),
        // with every 5th triggering a retry (240 total attempt starts) under paused clock.
        let mut ok_count = 0usize;
        for batch in 0..4 {
            let mut batch_handles = Vec::with_capacity(50);
            for j in 0..50 {
                let idx = batch * 50 + j;
                let p = Arc::clone(&pacer);
                let s = Arc::clone(&starts);
                let tries = Arc::new(AtomicUsize::new(0));
                batch_handles.push(tokio::spawn(async move {
                    p.run(
                        format!("burst-{idx}"),
                        RequestPriority::Visible,
                        move || {
                            let s = Arc::clone(&s);
                            let tries = Arc::clone(&tries);
                            async move {
                                s.lock().await.push(Instant::now());
                                let attempt = tries.fetch_add(1, Ordering::SeqCst);
                                if idx % 5 == 0 && attempt == 0 {
                                    Ok(AttemptOutcome::RetryableTransient(
                                        ProviderError::Upstream { status: 503 },
                                    ))
                                } else {
                                    Ok(AttemptOutcome::Success(vec![idx as u8]))
                                }
                            }
                        },
                    )
                    .await
                }));
            }
            for h in batch_handles {
                if h.await.unwrap().is_ok() {
                    ok_count += 1;
                }
            }
        }
        assert_eq!(ok_count, 200);

        let mut log = starts.lock().await.clone();
        log.sort();
        assert_eq!(
            log.len(),
            240,
            "expected 200 initial + 40 retry starts, got {}",
            log.len()
        );

        for pair in log.windows(2) {
            assert!(
                pair[1].duration_since(pair[0]) >= Duration::from_millis(1500),
                "spacing violation: {:?}",
                pair[1].duration_since(pair[0])
            );
        }

        // Sliding 60s window check (inclusive): no 60s span may contain > 40 starts.
        let mut left = 0usize;
        for right in 0..log.len() {
            while log[right].duration_since(log[left]) > Duration::from_secs(60) {
                left += 1;
            }
            let count_in_window = right - left + 1;
            assert!(
                count_in_window <= 40,
                "rolling 60s window exceeded 40 starts: {count_in_window}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn enforces_in_flight_cap_of_2() {
        let pacer = Arc::new(ApiPacer::new());
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for i in 0..6 {
            let p = Arc::clone(&pacer);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            handles.push(tokio::spawn(async move {
                p.run(
                    format!("slow-{i}"),
                    RequestPriority::Interactive,
                    move || {
                        let active = Arc::clone(&active);
                        let max_active = Arc::clone(&max_active);
                        async move {
                            let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                            max_active.fetch_max(cur, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_secs(6)).await;
                            active.fetch_sub(1, Ordering::SeqCst);
                            Ok(AttemptOutcome::Success(vec![1]))
                        }
                    },
                )
                .await
                .unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(max_active.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_queue_rejects_beyond_16_with_busy() {
        let pacer = Arc::new(ApiPacer::new());

        // Hold 2 in-flight requests for 60s so the next 16 fill the Interactive queue.
        let mut blockers = Vec::new();
        for b in 0..2 {
            let p = Arc::clone(&pacer);
            blockers.push(tokio::spawn(async move {
                p.run(
                    format!("blocker-{b}"),
                    RequestPriority::Interactive,
                    || async {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        Ok(AttemptOutcome::Success(vec![]))
                    },
                )
                .await
            }));
            tokio::time::sleep(Duration::from_millis(1600)).await;
        }

        let mut queued = Vec::new();
        for q in 0..16 {
            let p = Arc::clone(&pacer);
            queued.push(tokio::spawn(async move {
                p.run(format!("q-{q}"), RequestPriority::Interactive, || async {
                    Ok(AttemptOutcome::Success(vec![1]))
                })
                .await
            }));
        }
        tokio::task::yield_now().await;

        let overflow = pacer
            .run(
                "overflow-17".to_string(),
                RequestPriority::Interactive,
                || async { Ok(AttemptOutcome::Success(vec![1])) },
            )
            .await;
        assert_eq!(overflow, Err(ProviderError::Busy));

        for b in blockers {
            b.abort();
        }
        for q in queued {
            q.abort();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn visible_and_prefetch_queues_drop_oldest_beyond_64() {
        for priority in [RequestPriority::Visible, RequestPriority::Prefetch] {
            let pacer = Arc::new(ApiPacer::new());

            // Hold both in-flight slots so queued entries accumulate.
            let mut blockers = Vec::new();
            for b in 0..2 {
                let p = Arc::clone(&pacer);
                blockers.push(tokio::spawn(async move {
                    p.run(format!("blk-{b}"), RequestPriority::Interactive, || async {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        Ok(AttemptOutcome::Success(vec![]))
                    })
                    .await
                }));
                tokio::time::sleep(Duration::from_millis(1600)).await;
            }

            let mut queued = Vec::new();
            for i in 0..65 {
                let p = Arc::clone(&pacer);
                queued.push(tokio::spawn(async move {
                    p.run(format!("item-{i}"), priority, move || async move {
                        Ok(AttemptOutcome::Success(vec![i as u8]))
                    })
                    .await
                }));
                tokio::task::yield_now().await;
            }

            // Oldest item (#0) must have been evicted with Busy.
            let oldest = queued.remove(0).await.unwrap();
            assert_eq!(oldest, Err(ProviderError::Busy));

            // Remaining 64 items (#1..=64) must succeed once blockers finish.
            for b in blockers {
                let _ = b.await;
            }
            for h in queued {
                assert!(h.await.unwrap().is_ok());
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn coalesces_identical_in_queue_requests() {
        let pacer = Arc::new(ApiPacer::new());
        let executions = Arc::new(AtomicUsize::new(0));

        // Occupy pacer spacing so the 5 identical queued requests sit in queue together.
        let p_blk = Arc::clone(&pacer);
        let blk = tokio::spawn(async move {
            p_blk
                .run(
                    "lead-blk".to_string(),
                    RequestPriority::Interactive,
                    || async {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        Ok(AttemptOutcome::Success(vec![99]))
                    },
                )
                .await
        });
        tokio::task::yield_now().await;

        let mut waiters = Vec::new();
        for _ in 0..5 {
            let p = Arc::clone(&pacer);
            let ex = Arc::clone(&executions);
            waiters.push(tokio::spawn(async move {
                p.run(
                    "same-key".to_string(),
                    RequestPriority::Visible,
                    move || {
                        let ex = Arc::clone(&ex);
                        async move {
                            ex.fetch_add(1, Ordering::SeqCst);
                            Ok(AttemptOutcome::Success(vec![7, 8, 9]))
                        }
                    },
                )
                .await
            }));
        }

        let _ = blk.await;
        for w in waiters {
            assert_eq!(w.await.unwrap(), Ok(vec![7, 8, 9]));
        }
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_429_triggers_shared_cooldown_and_drops_prefetch() {
        let pacer = Arc::new(ApiPacer::new());
        let attempts = Arc::new(AtomicUsize::new(0));
        let (started_tx, mut started_rx) = starts_channel();
        let p_first = Arc::clone(&pacer);
        let a_first = Arc::clone(&attempts);

        let started_at = Instant::now();
        let vis = tokio::spawn(async move {
            p_first
                .run("vis-1".to_string(), RequestPriority::Visible, move || {
                    let a = Arc::clone(&a_first);
                    let started_tx = started_tx.clone();
                    async move {
                        let _ = started_tx.send(Instant::now());
                        let n = a.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            Ok(AttemptOutcome::RetryableRateLimit {
                                retry_after: Some(Duration::from_secs(10)),
                            })
                        } else {
                            Ok(AttemptOutcome::Success(vec![42]))
                        }
                    }
                })
                .await
        });

        let _ = tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
            .await
            .expect("first attempt must start")
            .expect("start signal");

        // Observe the applied cooldown instead of sleeping for a fixed time.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if pacer
                    .shared
                    .lock_state()
                    .cooldown_remaining(Instant::now())
                    .is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("429 must activate the shared cooldown");

        let pref_res = pacer
            .run("pref-1".to_string(), RequestPriority::Prefetch, || async {
                Ok(AttemptOutcome::Success(vec![1]))
            })
            .await;
        assert!(matches!(pref_res, Err(ProviderError::RateLimited { .. })));

        let vis_res = tokio::time::timeout(Duration::from_secs(60), vis)
            .await
            .expect("visible job must finish")
            .unwrap();
        assert_eq!(vis_res, Ok(vec![42]));
        assert!(
            started_at.elapsed() >= Duration::from_secs(10),
            "retry happened before 10s cooldown elapsed: {:?}",
            started_at.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn second_queued_attempt_starts_while_first_is_in_flight() {
        let pacer = Arc::new(ApiPacer::new());
        let (started_tx, mut started_rx) = starts_channel();
        let held = Arc::new(Notify::new());

        // Primer establishes spacing and completes immediately without signalling.
        pacer
            .run(
                "primer".to_string(),
                RequestPriority::Interactive,
                || async { Ok(AttemptOutcome::Success(vec![0])) },
            )
            .await
            .unwrap();

        let first = {
            let p = Arc::clone(&pacer);
            let started_tx = started_tx.clone();
            let held = Arc::clone(&held);
            tokio::spawn(async move {
                p.run(
                    "follower-1".to_string(),
                    RequestPriority::Interactive,
                    move || {
                        let started_tx = started_tx.clone();
                        let held = Arc::clone(&held);
                        async move {
                            let _ = started_tx.send(Instant::now());
                            held.notified().await;
                            Ok(AttemptOutcome::Success(vec![1]))
                        }
                    },
                )
                .await
            })
        };
        let second = {
            let p = Arc::clone(&pacer);
            let started_tx = started_tx.clone();
            let held = Arc::clone(&held);
            tokio::spawn(async move {
                p.run(
                    "follower-2".to_string(),
                    RequestPriority::Interactive,
                    move || {
                        let started_tx = started_tx.clone();
                        let held = Arc::clone(&held);
                        async move {
                            let _ = started_tx.send(Instant::now());
                            held.notified().await;
                            Ok(AttemptOutcome::Success(vec![2]))
                        }
                    },
                )
                .await
            })
        };
        tokio::task::yield_now().await;

        let t0 = tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
            .await
            .expect("follower 1 must start")
            .expect("start signal");
        let t1 = tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
            .await
            .expect("follower 2 must start while follower 1 is held")
            .expect("start signal");
        assert!(
            t1.duration_since(t0) >= MIN_SPACING,
            "follower 2 started before legal spacing: {:?}",
            t1.duration_since(t0)
        );

        let in_flight = pacer.shared.lock_state().in_flight;
        assert_eq!(in_flight, 2, "expected exactly two overlapping attempts");

        let third = {
            let p = Arc::clone(&pacer);
            let started_tx = started_tx.clone();
            tokio::spawn(async move {
                p.run(
                    "follower-3".to_string(),
                    RequestPriority::Interactive,
                    move || {
                        let started_tx = started_tx.clone();
                        async move {
                            let _ = started_tx.send(Instant::now());
                            Ok(AttemptOutcome::Success(vec![3]))
                        }
                    },
                )
                .await
            })
        };
        tokio::task::yield_now().await;
        assert!(
            tokio::time::timeout(Duration::from_secs(30), started_rx.recv())
                .await
                .is_err(),
            "a third attempt started while both slots were occupied"
        );

        held.notify_waiters();
        let first_result = tokio::time::timeout(Duration::from_secs(30), first)
            .await
            .expect("follower 1 completes")
            .unwrap();
        assert_eq!(first_result, Ok(vec![1]));

        let _third_start = tokio::time::timeout(Duration::from_secs(30), started_rx.recv())
            .await
            .expect("third attempt starts once a slot opens")
            .expect("start signal");

        let second_result = tokio::time::timeout(Duration::from_secs(30), second)
            .await
            .expect("follower 2 completes")
            .unwrap();
        assert_eq!(second_result, Ok(vec![2]));
        let third_result = tokio::time::timeout(Duration::from_secs(30), third)
            .await
            .expect("follower 3 completes")
            .unwrap();
        assert_eq!(third_result, Ok(vec![3]));
    }

    #[tokio::test(start_paused = true)]
    async fn huge_retry_after_never_panics_or_shortens_cooldown() {
        let pacer = Arc::new(ApiPacer::new());
        let attempts = Arc::new(AtomicUsize::new(0));
        let (seen_tx, mut seen_rx) = starts_channel();
        let (release_huge_tx, release_huge_rx) = oneshot::channel::<()>();
        let release_huge_rx = Arc::new(Mutex::new(Some(release_huge_rx)));
        let (release_short_tx, release_short_rx) = oneshot::channel::<()>();
        let release_short_rx = Arc::new(Mutex::new(Some(release_short_rx)));

        // Both attempts start before either 429 is signalled, so the smaller 429 is produced
        // by an attempt that was already in flight while the huge cooldown became active.
        let huge = {
            let p = Arc::clone(&pacer);
            let attempts = Arc::clone(&attempts);
            let seen_tx = seen_tx.clone();
            let release_huge_rx = Arc::clone(&release_huge_rx);
            tokio::spawn(async move {
                p.run("huge".to_string(), RequestPriority::Visible, move || {
                    let attempts = Arc::clone(&attempts);
                    let seen_tx = seen_tx.clone();
                    let release_huge_rx = Arc::clone(&release_huge_rx);
                    async move {
                        let _ = seen_tx.send(Instant::now());
                        attempts.fetch_add(1, Ordering::SeqCst);
                        let rx = release_huge_rx.lock().await.take();
                        if let Some(rx) = rx {
                            let _ = rx.await;
                        }
                        Ok(AttemptOutcome::RetryableRateLimit {
                            retry_after: Some(Duration::MAX),
                        })
                    }
                })
                .await
            })
        };

        let huge_started = tokio::time::timeout(Duration::from_secs(5), seen_rx.recv())
            .await
            .expect("first attempt starts")
            .expect("start signal");

        let (shorter, short_started) = {
            let p = Arc::clone(&pacer);
            let seen_tx = seen_tx.clone();
            let release_short_rx = Arc::clone(&release_short_rx);
            let handle = tokio::spawn(async move {
                p.run(
                    "short".to_string(),
                    RequestPriority::Interactive,
                    move || {
                        let seen_tx = seen_tx.clone();
                        let release_short_rx = Arc::clone(&release_short_rx);
                        async move {
                            let _ = seen_tx.send(Instant::now());
                            let rx = release_short_rx.lock().await.take();
                            if let Some(rx) = rx {
                                let _ = rx.await;
                            }
                            Ok(AttemptOutcome::RetryableRateLimit {
                                retry_after: Some(Duration::from_secs(5)),
                            })
                        }
                    },
                )
                .await
            });
            let started = tokio::time::timeout(Duration::from_secs(10), seen_rx.recv())
                .await
                .expect("second attempt starts")
                .expect("start signal");
            (handle, started)
        };
        assert!(
            short_started.duration_since(huge_started) >= MIN_SPACING,
            "the second attempt ignored the minimum spacing: {:?}",
            short_started.duration_since(huge_started)
        );

        release_huge_tx.send(()).expect("release the huge attempt");
        let (huge_deadline, remaining) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let observed = {
                    let state = pacer.shared.lock_state();
                    (
                        state.cooldown_until,
                        state.cooldown_remaining(Instant::now()),
                    )
                };
                if let (Some(deadline), Some(left)) = observed {
                    break (deadline, left);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the huge 429 must activate the cooldown");
        assert_eq!(
            remaining, MAX_COOLDOWN,
            "a huge header must clamp to one hour"
        );

        release_short_tx
            .send(())
            .expect("release the shorter attempt");
        let deadline_after_short = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let observed = {
                    let state = pacer.shared.lock_state();
                    (state.in_flight, state.cooldown_until)
                };
                if observed.0 == 0 {
                    break observed.1;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the shorter 429 resolves its attempt");
        assert_eq!(
            deadline_after_short,
            Some(huge_deadline),
            "a smaller 429 must not shorten or clear the cooldown"
        );

        // No retry may begin before the clamped one-hour cooldown.
        assert!(
            tokio::time::timeout(Duration::from_secs(59 * 60), seen_rx.recv())
                .await
                .is_err(),
            "retry started before the clamped one-hour cooldown"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        huge.abort();
        shorter.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn queued_prefetch_is_drained_on_429() {
        for retry_after in [Duration::from_secs(30), Duration::ZERO] {
            let pacer = Arc::new(ApiPacer::new());
            let attempts = Arc::new(AtomicUsize::new(0));
            let (release_tx, release_rx) = oneshot::channel::<()>();
            let release_rx = Arc::new(Mutex::new(Some(release_rx)));

            let holder = {
                let p = Arc::clone(&pacer);
                let release_rx = Arc::clone(&release_rx);
                let attempts = Arc::clone(&attempts);
                tokio::spawn(async move {
                    p.run(
                        "holder".to_string(),
                        RequestPriority::Interactive,
                        move || {
                            let release_rx = Arc::clone(&release_rx);
                            let attempts = Arc::clone(&attempts);
                            async move {
                                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                                if attempt == 0 {
                                    let rx = release_rx.lock().await.take();
                                    if let Some(rx) = rx {
                                        let _ = rx.await;
                                    }
                                    Ok(AttemptOutcome::RetryableRateLimit {
                                        retry_after: Some(retry_after),
                                    })
                                } else {
                                    Ok(AttemptOutcome::Success(vec![0]))
                                }
                            }
                        },
                    )
                    .await
                })
            };
            tokio::task::yield_now().await;

            let prefetch_a = {
                let p = Arc::clone(&pacer);
                tokio::spawn(async move {
                    p.run("dup".to_string(), RequestPriority::Prefetch, || async {
                        Ok(AttemptOutcome::Success(vec![1]))
                    })
                    .await
                })
            };
            tokio::task::yield_now().await;
            let prefetch_b = {
                let p = Arc::clone(&pacer);
                tokio::spawn(async move {
                    p.run("dup".to_string(), RequestPriority::Prefetch, || async {
                        Ok(AttemptOutcome::Success(vec![2]))
                    })
                    .await
                })
            };
            tokio::task::yield_now().await;
            let visible = {
                let p = Arc::clone(&pacer);
                tokio::spawn(async move {
                    p.run("visible".to_string(), RequestPriority::Visible, || async {
                        Ok(AttemptOutcome::Success(vec![3]))
                    })
                    .await
                })
            };
            tokio::task::yield_now().await;

            release_tx.send(()).expect("release holder");

            let a = tokio::time::timeout(Duration::from_secs(30), prefetch_a)
                .await
                .expect("prefetch a resolves")
                .unwrap();
            let b = tokio::time::timeout(Duration::from_secs(30), prefetch_b)
                .await
                .expect("prefetch b resolves")
                .unwrap();
            assert!(
                matches!(a, Err(ProviderError::RateLimited { .. })),
                "queued prefetch must be dropped on 429: {a:?}"
            );
            assert_eq!(a, b, "both prefetch subscribers must see the same result");
            assert_eq!(
                attempts.load(Ordering::SeqCst),
                1,
                "prefetch must never be attempted"
            );

            // Visible work stays pending until the deadline and then completes normally.
            let visible = tokio::time::timeout(Duration::from_secs(120), visible)
                .await
                .expect("visible resolves after the cooldown")
                .unwrap();
            assert_eq!(visible, Ok(vec![3]));

            let _ = tokio::time::timeout(Duration::from_secs(20), holder)
                .await
                .expect("holder finishes");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_duplicate_promotes_queued_prefetch() {
        let pacer = Arc::new(ApiPacer::new());
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let (promoted_tx, mut promoted_rx) = starts_channel();
        let executions = Arc::new(AtomicUsize::new(0));

        let holder = {
            let p = Arc::clone(&pacer);
            let release_rx = Arc::clone(&release_rx);
            tokio::spawn(async move {
                p.run("holder".to_string(), RequestPriority::Visible, move || {
                    let release_rx = Arc::clone(&release_rx);
                    async move {
                        let rx = release_rx.lock().await.take();
                        if let Some(rx) = rx {
                            let _ = rx.await;
                        }
                        Ok(AttemptOutcome::RetryableRateLimit {
                            retry_after: Some(Duration::from_secs(20)),
                        })
                    }
                })
                .await
            })
        };
        tokio::task::yield_now().await;

        let prefetch = {
            let p = Arc::clone(&pacer);
            let promoted_tx = promoted_tx.clone();
            let executions = Arc::clone(&executions);
            tokio::spawn(async move {
                p.run(
                    "promoted".to_string(),
                    RequestPriority::Prefetch,
                    move || {
                        let promoted_tx = promoted_tx.clone();
                        let executions = Arc::clone(&executions);
                        async move {
                            let _ = promoted_tx.send(Instant::now());
                            let attempt = executions.fetch_add(1, Ordering::SeqCst);
                            if attempt == 0 {
                                Ok(AttemptOutcome::RetryableRateLimit {
                                    retry_after: Some(Duration::from_secs(20)),
                                })
                            } else {
                                Ok(AttemptOutcome::Success(vec![9]))
                            }
                        }
                    },
                )
                .await
            })
        };
        tokio::task::yield_now().await;

        let visible = {
            let p = Arc::clone(&pacer);
            tokio::spawn(async move {
                p.run("visible".to_string(), RequestPriority::Visible, || async {
                    Ok(AttemptOutcome::Success(vec![4]))
                })
                .await
            })
        };
        tokio::task::yield_now().await;

        // Promote the queued prefetch job before the 429 drain runs.
        let promoting_caller = {
            let p = Arc::clone(&pacer);
            tokio::spawn(async move {
                p.run(
                    "promoted".to_string(),
                    RequestPriority::Interactive,
                    || async { Ok(AttemptOutcome::Success(vec![9])) },
                )
                .await
            })
        };
        tokio::task::yield_now().await;

        match pacer
            .shared
            .lock_state()
            .jobs
            .values()
            .find(|job| job.key == "promoted")
        {
            Some(job) => assert_eq!(
                job.priority,
                RequestPriority::Interactive,
                "an interactive duplicate must promote the queued prefetch job"
            ),
            None => panic!("promoted job disappeared before the drain"),
        }

        release_tx.send(()).expect("release holder");

        let prefetch_result = tokio::time::timeout(Duration::from_secs(300), prefetch)
            .await
            .expect("the promoted job survives the drain")
            .unwrap();
        assert_eq!(prefetch_result, Ok(vec![9]));

        let join_result = tokio::time::timeout(Duration::from_secs(300), promoting_caller)
            .await
            .expect("the promoting caller survives the drain")
            .unwrap();
        assert_eq!(
            join_result,
            Ok(vec![9]),
            "both subscribers share one result"
        );

        let visible_result = tokio::time::timeout(Duration::from_secs(300), visible)
            .await
            .expect("visible resolves")
            .unwrap();
        assert_eq!(visible_result, Ok(vec![4]));

        // The promoted job retried the 429 as Interactive with one shared execution per try.
        let starts = drain_starts(&mut promoted_rx).await;
        assert_eq!(starts.len(), 2, "promoted job must start exactly twice");
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        let _ = tokio::time::timeout(Duration::from_secs(20), holder).await;
    }

    #[tokio::test(start_paused = true)]
    async fn promotion_respects_destination_queue_capacity() {
        // A full Interactive queue rejects the promotion and leaves the original queued.
        {
            let pacer = Arc::new(ApiPacer::new());
            let (release_tx, release_rx) = oneshot::channel::<()>();
            let release_rx = Arc::new(Mutex::new(Some(release_rx)));
            let mut blockers = Vec::new();
            for b in 0..2 {
                let p = Arc::clone(&pacer);
                let release_rx = Arc::clone(&release_rx);
                blockers.push(tokio::spawn(async move {
                    p.run(
                        format!("blocker-{b}"),
                        RequestPriority::Interactive,
                        move || {
                            let release_rx = Arc::clone(&release_rx);
                            async move {
                                let rx = release_rx.lock().await.take();
                                if let Some(rx) = rx {
                                    let _ = rx.await;
                                }
                                Ok(AttemptOutcome::Success(vec![]))
                            }
                        },
                    )
                    .await
                }));
                tokio::task::yield_now().await;
            }

            let prefetch = {
                let p = Arc::clone(&pacer);
                tokio::spawn(async move {
                    p.run("dupe".to_string(), RequestPriority::Prefetch, || async {
                        Ok(AttemptOutcome::Success(vec![8]))
                    })
                    .await
                })
            };
            tokio::task::yield_now().await;

            let mut filler = Vec::new();
            for i in 0..INTERACTIVE_CAP {
                let p = Arc::clone(&pacer);
                filler.push(tokio::spawn(async move {
                    p.run(
                        format!("fill-{i}"),
                        RequestPriority::Interactive,
                        move || async move { Ok(AttemptOutcome::Success(vec![i as u8])) },
                    )
                    .await
                }));
                tokio::task::yield_now().await;
            }

            let rejected = pacer
                .run("dupe".to_string(), RequestPriority::Interactive, || async {
                    Ok(AttemptOutcome::Success(vec![8]))
                })
                .await;
            assert_eq!(
                rejected,
                Err(ProviderError::Busy),
                "a full Interactive queue must reject the promotion"
            );

            let still_queued = {
                let state = pacer.shared.lock_state();
                state
                    .jobs
                    .values()
                    .find(|job| job.key == "dupe")
                    .map(|job| job.priority)
            };
            assert_eq!(
                still_queued,
                Some(RequestPriority::Prefetch),
                "the original job must be left unchanged"
            );

            release_tx.send(()).expect("release blockers");
            for b in blockers {
                let _ = tokio::time::timeout(Duration::from_secs(10), b).await;
            }
            let _ = tokio::time::timeout(Duration::from_secs(180), prefetch)
                .await
                .expect("prefetch resolves")
                .unwrap();
            for f in filler {
                let _ = tokio::time::timeout(Duration::from_secs(180), f).await;
            }
        }

        // Promoting into a full Visible queue evicts its oldest entry.
        {
            let pacer = Arc::new(ApiPacer::new());
            let (release_tx, release_rx) = oneshot::channel::<()>();
            let release_rx = Arc::new(Mutex::new(Some(release_rx)));
            let mut blockers = Vec::new();
            for b in 0..2 {
                let p = Arc::clone(&pacer);
                let release_rx = Arc::clone(&release_rx);
                blockers.push(tokio::spawn(async move {
                    p.run(
                        format!("hold-{b}"),
                        RequestPriority::Interactive,
                        move || {
                            let release_rx = Arc::clone(&release_rx);
                            async move {
                                let rx = release_rx.lock().await.take();
                                if let Some(rx) = rx {
                                    let _ = rx.await;
                                }
                                Ok(AttemptOutcome::Success(vec![]))
                            }
                        },
                    )
                    .await
                }));
                tokio::task::yield_now().await;
            }

            let promotee = {
                let p = Arc::clone(&pacer);
                tokio::spawn(async move {
                    p.run(
                        "promotee".to_string(),
                        RequestPriority::Prefetch,
                        || async { Ok(AttemptOutcome::Success(vec![6])) },
                    )
                    .await
                })
            };
            tokio::task::yield_now().await;

            let mut visible = Vec::new();
            for i in 0..VISIBLE_CAP {
                let p = Arc::clone(&pacer);
                visible.push(tokio::spawn(async move {
                    p.run(
                        format!("vis-{i}"),
                        RequestPriority::Visible,
                        move || async move { Ok(AttemptOutcome::Success(vec![i as u8])) },
                    )
                    .await
                }));
                tokio::task::yield_now().await;
            }

            let promoted = pacer
                .run("promotee".to_string(), RequestPriority::Visible, || async {
                    Ok(AttemptOutcome::Success(vec![6]))
                })
                .await;
            assert_eq!(promoted, Ok(vec![6]));

            let oldest = tokio::time::timeout(Duration::from_secs(60), visible.remove(0))
                .await
                .expect("the evicted Visible subscriber is notified")
                .unwrap();
            assert_eq!(oldest, Err(ProviderError::Busy));

            release_tx.send(()).expect("release blockers");
            for b in blockers {
                let _ = tokio::time::timeout(Duration::from_secs(10), b).await;
            }
            let _ = tokio::time::timeout(Duration::from_secs(600), promotee)
                .await
                .expect("promoted job resolves")
                .unwrap();
            for v in visible {
                let _ = tokio::time::timeout(Duration::from_secs(600), v).await;
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn coalesced_survivor_keeps_job_after_first_caller_cancels() {
        // Case 1: the first caller is dropped while the job is still queued.
        {
            let pacer = Arc::new(ApiPacer::new());
            let executions = Arc::new(AtomicUsize::new(0));
            let (release_tx, release_rx) = oneshot::channel::<()>();
            let release_rx = Arc::new(Mutex::new(Some(release_rx)));

            let blocker = {
                let p = Arc::clone(&pacer);
                let release_rx = Arc::clone(&release_rx);
                tokio::spawn(async move {
                    p.run(
                        "blocker".to_string(),
                        RequestPriority::Interactive,
                        move || {
                            let release_rx = Arc::clone(&release_rx);
                            async move {
                                let rx = release_rx.lock().await.take();
                                if let Some(rx) = rx {
                                    let _ = rx.await;
                                }
                                Ok(AttemptOutcome::Success(vec![]))
                            }
                        },
                    )
                    .await
                })
            };
            tokio::task::yield_now().await;

            let first = {
                let p = Arc::clone(&pacer);
                let executions = Arc::clone(&executions);
                tokio::spawn(async move {
                    p.run("shared".to_string(), RequestPriority::Visible, move || {
                        let executions = Arc::clone(&executions);
                        async move {
                            executions.fetch_add(1, Ordering::SeqCst);
                            Ok(AttemptOutcome::Success(vec![5]))
                        }
                    })
                    .await
                })
            };
            tokio::task::yield_now().await;
            let survivor = {
                let p = Arc::clone(&pacer);
                let executions = Arc::clone(&executions);
                tokio::spawn(async move {
                    p.run("shared".to_string(), RequestPriority::Visible, move || {
                        let executions = Arc::clone(&executions);
                        async move {
                            executions.fetch_add(1, Ordering::SeqCst);
                            Ok(AttemptOutcome::Success(vec![5]))
                        }
                    })
                    .await
                })
            };
            tokio::task::yield_now().await;

            first.abort();
            let _ = first.await;
            tokio::task::yield_now().await;

            release_tx.send(()).expect("release blocker");
            let _ = tokio::time::timeout(Duration::from_secs(20), blocker)
                .await
                .expect("blocker resolves");

            let result = tokio::time::timeout(Duration::from_secs(20), survivor)
                .await
                .expect("the surviving subscriber resolves")
                .unwrap();
            assert_eq!(result, Ok(vec![5]));
            assert_eq!(
                executions.load(Ordering::SeqCst),
                1,
                "only one job may execute"
            );
        }

        // Case 2: the first caller is dropped during a held attempt.
        {
            let pacer = Arc::new(ApiPacer::new());
            let executions = Arc::new(AtomicUsize::new(0));
            let (started_tx, mut started_rx) = starts_channel();
            let (release_tx, release_rx) = oneshot::channel::<()>();
            let release_rx = Arc::new(Mutex::new(Some(release_rx)));

            let blocker = {
                let p = Arc::clone(&pacer);
                let release_rx = Arc::clone(&release_rx);
                tokio::spawn(async move {
                    p.run(
                        "hold".to_string(),
                        RequestPriority::Interactive,
                        move || {
                            let release_rx = Arc::clone(&release_rx);
                            async move {
                                let rx = release_rx.lock().await.take();
                                if let Some(rx) = rx {
                                    let _ = rx.await;
                                }
                                Ok(AttemptOutcome::Success(vec![]))
                            }
                        },
                    )
                    .await
                })
            };
            tokio::task::yield_now().await;

            let first = {
                let p = Arc::clone(&pacer);
                let started_tx = started_tx.clone();
                let executions = Arc::clone(&executions);
                tokio::spawn(async move {
                    p.run(
                        "held".to_string(),
                        RequestPriority::Interactive,
                        move || {
                            let started_tx = started_tx.clone();
                            let executions = Arc::clone(&executions);
                            async move {
                                let _ = started_tx.send(Instant::now());
                                executions.fetch_add(1, Ordering::SeqCst);
                                Ok(AttemptOutcome::Success(vec![5]))
                            }
                        },
                    )
                    .await
                })
            };
            tokio::task::yield_now().await;
            let survivor = {
                let p = Arc::clone(&pacer);
                let executions = Arc::clone(&executions);
                tokio::spawn(async move {
                    p.run(
                        "held".to_string(),
                        RequestPriority::Interactive,
                        move || {
                            let executions = Arc::clone(&executions);
                            async move {
                                executions.fetch_add(1, Ordering::SeqCst);
                                Ok(AttemptOutcome::Success(vec![5]))
                            }
                        },
                    )
                    .await
                })
            };
            tokio::task::yield_now().await;

            // Let the shared job start and hold the attempt open.
            release_tx.send(()).expect("release blocker attempt");
            let _ = tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
                .await
                .expect("shared job starts")
                .expect("start signal");

            first.abort();
            let _ = first.await;
            tokio::task::yield_now().await;

            let result = tokio::time::timeout(Duration::from_secs(30), survivor)
                .await
                .expect("the surviving subscriber resolves")
                .unwrap();
            assert_eq!(result, Ok(vec![5]));
            assert_eq!(
                executions.load(Ordering::SeqCst),
                1,
                "only the shared job may execute"
            );
            let _ = tokio::time::timeout(Duration::from_secs(10), blocker).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn last_subscriber_cancellation_releases_queue_and_permit() {
        // Cancellation while queued: nothing starts, and a same-key job later still runs.
        {
            let pacer = Arc::new(ApiPacer::new());
            let executions = Arc::new(AtomicUsize::new(0));
            let (release_tx, release_rx) = oneshot::channel::<()>();
            let release_rx = Arc::new(Mutex::new(Some(release_rx)));

            let blocker = {
                let p = Arc::clone(&pacer);
                let release_rx = Arc::clone(&release_rx);
                tokio::spawn(async move {
                    p.run(
                        "blocker".to_string(),
                        RequestPriority::Interactive,
                        move || {
                            let release_rx = Arc::clone(&release_rx);
                            async move {
                                let rx = release_rx.lock().await.take();
                                if let Some(rx) = rx {
                                    let _ = rx.await;
                                }
                                Ok(AttemptOutcome::Success(vec![]))
                            }
                        },
                    )
                    .await
                })
            };
            tokio::task::yield_now().await;

            let cancelled = {
                let p = Arc::clone(&pacer);
                let executions = Arc::clone(&executions);
                tokio::spawn(async move {
                    p.run(
                        "same-key".to_string(),
                        RequestPriority::Visible,
                        move || {
                            let executions = Arc::clone(&executions);
                            async move {
                                executions.fetch_add(1, Ordering::SeqCst);
                                Ok(AttemptOutcome::Success(vec![1]))
                            }
                        },
                    )
                    .await
                })
            };
            tokio::task::yield_now().await;
            cancelled.abort();
            let _ = cancelled.await;
            tokio::task::yield_now().await;
            assert!(
                !pacer
                    .shared
                    .lock_state()
                    .jobs
                    .values()
                    .any(|job| job.key == "same-key"),
                "a queued job with no subscribers must be retired immediately"
            );

            release_tx.send(()).expect("release blocker");
            let _ = tokio::time::timeout(Duration::from_secs(20), blocker)
                .await
                .expect("blocker resolves");
            assert_eq!(
                executions.load(Ordering::SeqCst),
                0,
                "a cancelled queued job must never start"
            );

            let replacement = {
                let executions = Arc::clone(&executions);
                pacer
                    .run(
                        "same-key".to_string(),
                        RequestPriority::Visible,
                        move || {
                            let executions = Arc::clone(&executions);
                            async move {
                                executions.fetch_add(1, Ordering::SeqCst);
                                Ok(AttemptOutcome::Success(vec![2]))
                            }
                        },
                    )
                    .await
            };
            assert_eq!(replacement, Ok(vec![2]));
            assert_eq!(
                executions.load(Ordering::SeqCst),
                1,
                "stale cleanup must not affect the newer same-key job"
            );
            assert_eq!(pacer.shared.lock_state().in_flight, 0, "no slot may leak");
        }

        // Cancellation during a held attempt: the slot is released and new work can run.
        {
            let pacer = Arc::new(ApiPacer::new());
            let (started_tx, mut started_rx) = starts_channel();
            let (release_tx, release_rx) = oneshot::channel::<()>();
            let release_rx = Arc::new(Mutex::new(Some(release_rx)));

            let cancelled = {
                let p = Arc::clone(&pacer);
                let started_tx = started_tx.clone();
                let release_rx = Arc::clone(&release_rx);
                tokio::spawn(async move {
                    p.run(
                        "held-2".to_string(),
                        RequestPriority::Interactive,
                        move || {
                            let started_tx = started_tx.clone();
                            let release_rx = Arc::clone(&release_rx);
                            async move {
                                let _ = started_tx.send(Instant::now());
                                let rx = release_rx.lock().await.take();
                                if let Some(rx) = rx {
                                    let _ = rx.await;
                                }
                                Ok(AttemptOutcome::Success(vec![1]))
                            }
                        },
                    )
                    .await
                })
            };
            let _ = tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
                .await
                .expect("attempt starts")
                .expect("start signal");

            cancelled.abort();
            let _ = cancelled.await;
            tokio::task::yield_now().await;

            drop(release_tx);
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if pacer.shared.lock_state().in_flight == 0 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("a cancelled attempt must release its permit");

            let next = pacer
                .run("next".to_string(), RequestPriority::Interactive, || async {
                    Ok(AttemptOutcome::Success(vec![3]))
                })
                .await;
            assert_eq!(next, Ok(vec![3]));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn single_200_request_burst_honors_queue_and_attempt_limits() {
        let pacer = Arc::new(ApiPacer::new());
        let starts = Arc::new(Mutex::new(Vec::<Instant>::new()));

        let (blocker_tx, mut blocker_rx) = starts_channel();
        let mut blocker_releases = Vec::new();
        let mut blockers = Vec::new();
        for b in 0..2 {
            let p = Arc::clone(&pacer);
            let starts = Arc::clone(&starts);
            let blocker_tx = blocker_tx.clone();
            let (release_tx, release_rx) = oneshot::channel::<()>();
            blocker_releases.push(release_tx);
            let release_rx = Arc::new(Mutex::new(Some(release_rx)));
            blockers.push(tokio::spawn(async move {
                p.run(
                    format!("blocker-{b}"),
                    RequestPriority::Interactive,
                    move || {
                        let starts = Arc::clone(&starts);
                        let blocker_tx = blocker_tx.clone();
                        let release_rx = Arc::clone(&release_rx);
                        async move {
                            starts.lock().await.push(Instant::now());
                            let _ = blocker_tx.send(Instant::now());
                            let rx = release_rx.lock().await.take();
                            if let Some(rx) = rx {
                                let _ = rx.await;
                            }
                            Ok(AttemptOutcome::Success(vec![]))
                        }
                    },
                )
                .await
            }));
            tokio::time::timeout(Duration::from_secs(5), blocker_rx.recv())
                .await
                .expect("each blocker must actually start")
                .expect("start signal");
            tokio::time::sleep(MIN_SPACING).await;
        }
        assert_eq!(
            pacer.shared.lock_state().in_flight,
            2,
            "both blockers must hold a slot before the burst is submitted"
        );

        let mut interactive = Vec::new();
        for i in 0..20 {
            let p = Arc::clone(&pacer);
            let starts = Arc::clone(&starts);
            let tries = Arc::new(AtomicUsize::new(0));
            interactive.push(tokio::spawn(async move {
                p.run(
                    format!("int-{i}"),
                    RequestPriority::Interactive,
                    move || {
                        let starts = Arc::clone(&starts);
                        let tries = Arc::clone(&tries);
                        async move {
                            starts.lock().await.push(Instant::now());
                            if tries.fetch_add(1, Ordering::SeqCst) == 0 {
                                Ok(AttemptOutcome::RetryableTransient(
                                    ProviderError::Upstream { status: 503 },
                                ))
                            } else {
                                Ok(AttemptOutcome::Success(vec![i as u8]))
                            }
                        }
                    },
                )
                .await
            }));
            tokio::task::yield_now().await;
        }

        let mut visible = Vec::new();
        for i in 0..90 {
            let p = Arc::clone(&pacer);
            let starts = Arc::clone(&starts);
            let tries = Arc::new(AtomicUsize::new(0));
            visible.push(tokio::spawn(async move {
                p.run(format!("vis-{i}"), RequestPriority::Visible, move || {
                    let starts = Arc::clone(&starts);
                    let tries = Arc::clone(&tries);
                    async move {
                        starts.lock().await.push(Instant::now());
                        if tries.fetch_add(1, Ordering::SeqCst) == 0 {
                            Ok(AttemptOutcome::RetryableTransient(
                                ProviderError::Upstream { status: 503 },
                            ))
                        } else {
                            Ok(AttemptOutcome::Success(vec![i as u8]))
                        }
                    }
                })
                .await
            }));
            tokio::task::yield_now().await;
        }

        let mut prefetch = Vec::new();
        for i in 0..90 {
            let p = Arc::clone(&pacer);
            let starts = Arc::clone(&starts);
            let tries = Arc::new(AtomicUsize::new(0));
            prefetch.push(tokio::spawn(async move {
                p.run(format!("pre-{i}"), RequestPriority::Prefetch, move || {
                    let starts = Arc::clone(&starts);
                    let tries = Arc::clone(&tries);
                    async move {
                        starts.lock().await.push(Instant::now());
                        if tries.fetch_add(1, Ordering::SeqCst) == 0 {
                            Ok(AttemptOutcome::RetryableTransient(
                                ProviderError::Upstream { status: 503 },
                            ))
                        } else {
                            Ok(AttemptOutcome::Success(vec![i as u8]))
                        }
                    }
                })
                .await
            }));
            tokio::task::yield_now().await;
        }

        for tx in blocker_releases {
            tx.send(()).expect("release blockers");
        }
        for b in blockers {
            let _ = tokio::time::timeout(Duration::from_secs(10), b).await;
        }

        let mut rejected_interactive = 0usize;
        let mut admitted_interactive = 0usize;
        for handle in interactive {
            let result = tokio::time::timeout(Duration::from_secs(1200), handle)
                .await
                .expect("interactive resolves")
                .unwrap();
            match result {
                Ok(_) => admitted_interactive += 1,
                Err(ProviderError::Busy) => rejected_interactive += 1,
                other => panic!("unexpected interactive result: {other:?}"),
            }
        }
        assert_eq!(
            rejected_interactive, 4,
            "4 Interactive jobs must be rejected"
        );
        assert_eq!(admitted_interactive, 16);

        let mut evicted_visible = 0usize;
        let mut admitted_visible = 0usize;
        for handle in visible {
            let result = tokio::time::timeout(Duration::from_secs(1200), handle)
                .await
                .expect("visible resolves")
                .unwrap();
            match result {
                Ok(_) => admitted_visible += 1,
                Err(ProviderError::Busy) => evicted_visible += 1,
                other => panic!("unexpected visible result: {other:?}"),
            }
        }
        assert_eq!(
            evicted_visible, 26,
            "the oldest 26 Visible jobs must be evicted"
        );
        assert_eq!(admitted_visible, 64);

        let mut evicted_prefetch = 0usize;
        let mut admitted_prefetch = 0usize;
        for handle in prefetch {
            let result = tokio::time::timeout(Duration::from_secs(1200), handle)
                .await
                .expect("prefetch resolves")
                .unwrap();
            match result {
                Ok(_) => admitted_prefetch += 1,
                Err(ProviderError::Busy) => evicted_prefetch += 1,
                other => panic!("unexpected prefetch result: {other:?}"),
            }
        }
        assert_eq!(
            evicted_prefetch, 26,
            "the oldest 26 Prefetch jobs must be evicted"
        );
        assert_eq!(admitted_prefetch, 64);

        let mut log = starts.lock().await.clone();
        log.sort();
        assert_eq!(
            log.len(),
            290,
            "expected 2 blockers + 144 jobs x 2 attempts, got {}",
            log.len()
        );
        for pair in log.windows(2) {
            assert!(
                pair[1].duration_since(pair[0]) >= MIN_SPACING,
                "spacing violation: {:?}",
                pair[1].duration_since(pair[0])
            );
        }
        let mut left = 0usize;
        let mut peak = 0usize;
        for right in 0..log.len() {
            while log[right].duration_since(log[left]) > WINDOW_DURATION {
                left += 1;
            }
            peak = peak.max(right - left + 1);
        }
        assert!(peak <= WINDOW_CAP, "rolling 60s window exceeded: {peak}");
        assert_eq!(pacer.shared.lock_state().in_flight, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn transient_retries_back_off_and_exhaust_at_three_attempts() {
        let pacer = Arc::new(ApiPacer::new());
        let starts = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let attempts = Arc::new(AtomicUsize::new(0));

        let p = Arc::clone(&pacer);
        let s = Arc::clone(&starts);
        let a = Arc::clone(&attempts);
        let job = tokio::spawn(async move {
            p.run(
                "slow-fail".to_string(),
                RequestPriority::Visible,
                move || {
                    let s = Arc::clone(&s);
                    let a = Arc::clone(&a);
                    async move {
                        s.lock().await.push(Instant::now());
                        a.fetch_add(1, Ordering::SeqCst);
                        Ok(AttemptOutcome::RetryableTransient(ProviderError::Timeout))
                    }
                },
            )
            .await
        });

        let result = tokio::time::timeout(Duration::from_secs(120), job)
            .await
            .expect("the job must exhaust its retries")
            .unwrap();
        assert_eq!(result, Err(ProviderError::Timeout));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(pacer.shared.lock_state().in_flight, 0, "no slot may leak");

        let log = starts.lock().await.clone();
        assert_eq!(log.len(), 3);
        assert!(
            log[1].duration_since(log[0]) >= Duration::from_secs(2),
            "first retry backoff too short: {:?}",
            log[1].duration_since(log[0])
        );
        assert!(
            log[2].duration_since(log[1]) >= Duration::from_secs(4),
            "second retry backoff too short: {:?}",
            log[2].duration_since(log[1])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn three_rate_limits_exhaust_retries_and_keep_final_cooldown() {
        let pacer = Arc::new(ApiPacer::new());
        let attempts = Arc::new(AtomicUsize::new(0));

        let p = Arc::clone(&pacer);
        let a = Arc::clone(&attempts);
        let job = tokio::spawn(async move {
            p.run("limited".to_string(), RequestPriority::Visible, move || {
                let a = Arc::clone(&a);
                async move {
                    a.fetch_add(1, Ordering::SeqCst);
                    Ok(AttemptOutcome::RetryableRateLimit {
                        retry_after: Some(Duration::from_secs(5)),
                    })
                }
            })
            .await
        });

        let result = tokio::time::timeout(Duration::from_secs(300), job)
            .await
            .expect("job resolves")
            .unwrap();
        assert!(matches!(
            result,
            Err(ProviderError::RateLimited {
                retry_after: Some(_)
            })
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "no fourth attempt");

        assert!(
            pacer.shared.lock_state().cooldown_until.is_some(),
            "the final 429 must still set the shared cooldown"
        );

        let prefetch = pacer
            .run("after".to_string(), RequestPriority::Prefetch, || async {
                Ok(AttemptOutcome::Success(vec![]))
            })
            .await;
        assert!(
            matches!(prefetch, Err(ProviderError::RateLimited { .. })),
            "the final 429 must still affect other jobs: {prefetch:?}"
        );
        assert_eq!(pacer.shared.lock_state().in_flight, 0);
    }
}
