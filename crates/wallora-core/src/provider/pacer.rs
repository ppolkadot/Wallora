use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, oneshot};
use tokio::time::Instant;

use crate::domain::RequestPriority;
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

#[derive(Debug)]
pub enum AttemptOutcome {
    Success(Vec<u8>),
    RetryableRateLimit { retry_after: Option<Duration> },
    RetryableTransient(ProviderError),
    Terminal(ProviderError),
}

struct QueueEntry {
    seq: u64,
    key: String,
    subscribers: Vec<oneshot::Sender<Result<Vec<u8>, ProviderError>>>,
}

#[derive(Default)]
struct PacerState {
    next_seq: u64,
    interactive: VecDeque<QueueEntry>,
    visible: VecDeque<QueueEntry>,
    prefetch: VecDeque<QueueEntry>,
    drop_reason: HashMap<u64, ProviderError>,
    in_flight: usize,
    last_start: Option<Instant>,
    start_log: VecDeque<Instant>,
    cooldown_until: Option<Instant>,
}

impl PacerState {
    fn queue_mut(&mut self, priority: RequestPriority) -> &mut VecDeque<QueueEntry> {
        match priority {
            RequestPriority::Interactive => &mut self.interactive,
            RequestPriority::Visible => &mut self.visible,
            RequestPriority::Prefetch => &mut self.prefetch,
        }
    }

    fn queue_for_seq_mut(&mut self, priority: RequestPriority) -> &mut VecDeque<QueueEntry> {
        self.queue_mut(priority)
    }

    fn highest_front_seq(&self) -> Option<u64> {
        self.interactive
            .front()
            .or_else(|| self.visible.front())
            .or_else(|| self.prefetch.front())
            .map(|e| e.seq)
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
            // Strict rolling 60s window: wait just past 60s from oldest in window
            // so neither [T, T+60s] nor [T, T+60s) ever exceeds 40 starts.
            earliest = earliest.max(oldest + WINDOW_DURATION + Duration::from_millis(1));
        }
        if let Some(cd) = self.cooldown_until {
            earliest = earliest.max(cd);
        }
        earliest
    }

    fn pop_front_for_seq(&mut self, seq: u64) -> Option<QueueEntry> {
        for q in [&mut self.interactive, &mut self.visible, &mut self.prefetch] {
            if q.front().is_some_and(|e| e.seq == seq) {
                return q.pop_front();
            }
        }
        None
    }

    fn remove_by_seq(&mut self, seq: u64) -> Option<QueueEntry> {
        for q in [&mut self.interactive, &mut self.visible, &mut self.prefetch] {
            if let Some(pos) = q.iter().position(|e| e.seq == seq) {
                return q.remove(pos);
            }
        }
        None
    }

    fn find_by_key_mut(&mut self, key: &str) -> Option<&mut QueueEntry> {
        if let Some(entry) = self.interactive.iter_mut().find(|e| e.key == key) {
            return Some(entry);
        }
        if let Some(entry) = self.visible.iter_mut().find(|e| e.key == key) {
            return Some(entry);
        }
        self.prefetch.iter_mut().find(|e| e.key == key)
    }

    fn drain_prefetch_for_cooldown(&mut self, retry_after: Duration) {
        let err = ProviderError::RateLimited {
            retry_after: Some(retry_after),
        };
        while let Some(entry) = self.prefetch.pop_front() {
            for sub in entry.subscribers {
                let _ = sub.send(Err(err.clone()));
            }
            self.drop_reason.insert(entry.seq, err.clone());
        }
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

struct AttemptPermit {
    shared: Arc<PacerShared>,
}

impl Drop for AttemptPermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.in_flight = state.in_flight.saturating_sub(1);
        }
        self.shared.notify.notify_waiters();
    }
}

struct QueueMembershipGuard {
    shared: Arc<PacerShared>,
    seq: u64,
    armed: bool,
}

impl QueueMembershipGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for QueueMembershipGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut state) = self.shared.state.lock() {
            if let Some(entry) = state.remove_by_seq(self.seq) {
                for sub in entry.subscribers {
                    let _ = sub.send(Err(ProviderError::Busy));
                }
            }
            state.drop_reason.remove(&self.seq);
        }
        self.shared.notify.notify_waiters();
    }
}

enum EnqueueOutcome {
    Leader {
        seq: u64,
        guard: QueueMembershipGuard,
    },
    Waiter(oneshot::Receiver<Result<Vec<u8>, ProviderError>>),
}

/// Tokenless sliding-log pacer + three-tier priority queue + coalescing for Wallhaven API calls.
#[derive(Clone, Default)]
pub struct ApiPacer {
    shared: Arc<PacerShared>,
}

impl ApiPacer {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn run<F, Fut>(
        &self,
        coalesce_key: String,
        priority: RequestPriority,
        mut attempt_fn: F,
    ) -> Result<Vec<u8>, ProviderError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<AttemptOutcome, ProviderError>>,
    {
        let (seq, mut guard) = match self.enqueue_or_coalesce(coalesce_key, priority)? {
            EnqueueOutcome::Leader { seq, guard } => (seq, guard),
            EnqueueOutcome::Waiter(rx) => {
                return rx.await.unwrap_or(Err(ProviderError::Busy));
            }
        };

        let (mut subscribers, first_permit) = match self.wait_for_initial_slot(seq).await {
            Ok((subs, permit)) => {
                guard.disarm();
                (subs, permit)
            }
            Err(err) => {
                guard.disarm();
                return Err(err);
            }
        };

        let mut permit = Some(first_permit);
        let mut retry_idx = 0usize;

        let result = loop {
            let attempt_res = attempt_fn().await;
            drop(permit.take());

            match attempt_res {
                Ok(AttemptOutcome::Success(bytes)) => break Ok(bytes),
                Ok(AttemptOutcome::Terminal(err)) => break Err(err),
                Err(err) => break Err(err),
                Ok(AttemptOutcome::RetryableRateLimit { retry_after }) => {
                    let dur = self.activate_cooldown(retry_after);
                    if priority == RequestPriority::Prefetch {
                        break Err(ProviderError::RateLimited {
                            retry_after: Some(dur),
                        });
                    }
                    if retry_idx >= MAX_RETRIES {
                        break Err(ProviderError::RateLimited {
                            retry_after: Some(dur),
                        });
                    }
                    retry_idx += 1;
                    permit = Some(self.acquire_retry_slot(priority).await?);
                }
                Ok(AttemptOutcome::RetryableTransient(err)) => {
                    if retry_idx >= MAX_RETRIES {
                        break Err(err);
                    }
                    retry_idx += 1;
                    match self.acquire_retry_slot(priority).await {
                        Ok(p) => permit = Some(p),
                        Err(cooldown_err) => break Err(cooldown_err),
                    }
                }
            }
        };

        for sub in subscribers.drain(..) {
            let _ = sub.send(result.clone());
        }
        result
    }

    fn enqueue_or_coalesce(
        &self,
        key: String,
        priority: RequestPriority,
    ) -> Result<EnqueueOutcome, ProviderError> {
        let mut state = self.shared.state.lock().map_err(|_| ProviderError::Busy)?;
        let now = Instant::now();

        if priority == RequestPriority::Prefetch
            && let Some(until) = state.cooldown_until
            && until > now
        {
            return Err(ProviderError::RateLimited {
                retry_after: Some(until.duration_since(now)),
            });
        }

        if let Some(existing) = state.find_by_key_mut(&key) {
            let (tx, rx) = oneshot::channel();
            existing.subscribers.push(tx);
            return Ok(EnqueueOutcome::Waiter(rx));
        }

        match priority {
            RequestPriority::Interactive => {
                if state.interactive.len() >= INTERACTIVE_CAP {
                    return Err(ProviderError::Busy);
                }
            }
            RequestPriority::Visible => {
                while state.visible.len() >= VISIBLE_CAP {
                    if let Some(evicted) = state.visible.pop_front() {
                        for sub in evicted.subscribers {
                            let _ = sub.send(Err(ProviderError::Busy));
                        }
                        state.drop_reason.insert(evicted.seq, ProviderError::Busy);
                    }
                }
            }
            RequestPriority::Prefetch => {
                while state.prefetch.len() >= PREFETCH_CAP {
                    if let Some(evicted) = state.prefetch.pop_front() {
                        for sub in evicted.subscribers {
                            let _ = sub.send(Err(ProviderError::Busy));
                        }
                        state.drop_reason.insert(evicted.seq, ProviderError::Busy);
                    }
                }
            }
        }

        let seq = state.next_seq;
        state.next_seq = state.next_seq.wrapping_add(1);
        state.queue_for_seq_mut(priority).push_back(QueueEntry {
            seq,
            key,
            subscribers: Vec::new(),
        });

        self.shared.notify.notify_waiters();

        Ok(EnqueueOutcome::Leader {
            seq,
            guard: QueueMembershipGuard {
                shared: Arc::clone(&self.shared),
                seq,
                armed: true,
            },
        })
    }

    async fn wait_for_initial_slot(
        &self,
        seq: u64,
    ) -> Result<
        (
            Vec<oneshot::Sender<Result<Vec<u8>, ProviderError>>>,
            AttemptPermit,
        ),
        ProviderError,
    > {
        loop {
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let wake_at: Option<Instant>;
            {
                let mut state = self.shared.state.lock().map_err(|_| ProviderError::Busy)?;
                if let Some(reason) = state.drop_reason.remove(&seq) {
                    return Err(reason);
                }

                let now = Instant::now();
                let is_front = state.highest_front_seq() == Some(seq);
                if is_front && state.in_flight < MAX_IN_FLIGHT {
                    let earliest = state.earliest_attempt_start(now);
                    if now >= earliest {
                        let entry = state.pop_front_for_seq(seq).ok_or(ProviderError::Busy)?;
                        state.in_flight += 1;
                        state.last_start = Some(now);
                        state.start_log.push_back(now);
                        return Ok((
                            entry.subscribers,
                            AttemptPermit {
                                shared: Arc::clone(&self.shared),
                            },
                        ));
                    }
                    wake_at = Some(earliest);
                } else {
                    wake_at = None;
                }
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

    fn activate_cooldown(&self, retry_after: Option<Duration>) -> Duration {
        let dur = retry_after.unwrap_or(DEFAULT_COOLDOWN);
        if let Ok(mut state) = self.shared.state.lock() {
            let now = Instant::now();
            let until = now + dur;
            state.cooldown_until = Some(state.cooldown_until.map_or(until, |prev| prev.max(until)));
            state.drain_prefetch_for_cooldown(dur);
        }
        self.shared.notify.notify_waiters();
        dur
    }

    async fn acquire_retry_slot(
        &self,
        priority: RequestPriority,
    ) -> Result<AttemptPermit, ProviderError> {
        loop {
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let wake_at: Option<Instant>;
            {
                let mut state = self.shared.state.lock().map_err(|_| ProviderError::Busy)?;
                let now = Instant::now();
                if priority == RequestPriority::Prefetch
                    && let Some(until) = state.cooldown_until
                    && until > now
                {
                    return Err(ProviderError::RateLimited {
                        retry_after: Some(until.duration_since(now)),
                    });
                }

                if state.in_flight < MAX_IN_FLIGHT {
                    let earliest = state.earliest_attempt_start(now);
                    if now >= earliest {
                        state.in_flight += 1;
                        state.last_start = Some(now);
                        state.start_log.push_back(now);
                        return Ok(AttemptPermit {
                            shared: Arc::clone(&self.shared),
                        });
                    }
                    wake_at = Some(earliest);
                } else {
                    wake_at = None;
                }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;
    use tokio::time::{Duration, Instant};

    #[tokio::test(start_paused = true)]
    async fn enforces_minimum_spacing_of_1500ms_between_starts() {
        let pacer = Arc::new(ApiPacer::new());
        let starts = Arc::new(Mutex::new(Vec::<Instant>::new()));

        let mut handles = Vec::new();
        for i in 0..5 {
            let p = Arc::clone(&pacer);
            let s = Arc::clone(&starts);
            handles.push(tokio::spawn(async move {
                p.run(format!("req-{i}"), RequestPriority::Interactive, || {
                    let s = Arc::clone(&s);
                    async move {
                        s.lock().await.push(Instant::now());
                        Ok(AttemptOutcome::Success(vec![1]))
                    }
                })
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

        // Run 4 contiguous bursts of 50 Visible requests (200 total requests),
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
                    p.run(format!("burst-{idx}"), RequestPriority::Visible, || {
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
                    })
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
                p.run(format!("slow-{i}"), RequestPriority::Interactive, || {
                    let active = Arc::clone(&active);
                    let max_active = Arc::clone(&max_active);
                    async move {
                        let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_active.fetch_max(cur, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_secs(6)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(AttemptOutcome::Success(vec![1]))
                    }
                })
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
                    p.run(format!("item-{i}"), priority, || async move {
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
                p.run("same-key".to_string(), RequestPriority::Visible, || {
                    let ex = Arc::clone(&ex);
                    async move {
                        ex.fetch_add(1, Ordering::SeqCst);
                        Ok(AttemptOutcome::Success(vec![7, 8, 9]))
                    }
                })
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
        let p_first = Arc::clone(&pacer);
        let a_first = Arc::clone(&attempts);

        let started_at = Instant::now();
        let vis = tokio::spawn(async move {
            p_first
                .run("vis-1".to_string(), RequestPriority::Visible, || {
                    let a = Arc::clone(&a_first);
                    async move {
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

        tokio::task::yield_now().await;

        let pref_res = pacer
            .run("pref-1".to_string(), RequestPriority::Prefetch, || async {
                Ok(AttemptOutcome::Success(vec![1]))
            })
            .await;
        assert!(matches!(pref_res, Err(ProviderError::RateLimited { .. })));

        let vis_res = vis.await.unwrap();
        assert_eq!(vis_res, Ok(vec![42]));
        assert!(
            started_at.elapsed() >= Duration::from_secs(10),
            "retry happened before 10s cooldown elapsed: {:?}",
            started_at.elapsed()
        );
    }
}
