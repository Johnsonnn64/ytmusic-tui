//! Automatic radio selection, independent of mpv and HTTP. The runtime owns
//! this state and applies its playback decisions; fetches return tagged results
//! so an old queue can never restart playback after a replacement.

use std::collections::{HashSet, VecDeque};

use crate::queue::{QueueManager, RepeatMode, Track};

const RECENT_LIMIT: usize = 100;
const MAX_ATTEMPTS: u8 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub generation: u64,
    pub seed: String,
    pub attempt: u8,
    pub excluded: HashSet<String>,
}

#[derive(Debug)]
pub(super) struct Failure {
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Advance {
    Play,
    Wait,
    Idle,
}

#[derive(Default)]
pub(super) struct Autoplay {
    generation: u64,
    active: bool,
    seed: Option<String>,
    pending: Option<Request>,
    prepared: Vec<Track>,
    attempts: u8,
    failure: Option<String>,
    notified: bool,
    waiting: bool,
    paused: bool,
    recent: VecDeque<String>,
}

impl Autoplay {
    /// Called for explicit playback, including a new radio station. History
    /// spans queues, but all pending work belongs to the replaced queue.
    pub fn reset(&mut self) {
        self.invalidate(None);
        self.active = true;
        self.waiting = false;
    }

    fn invalidate(&mut self, seed: Option<String>) {
        self.generation = self.generation.wrapping_add(1);
        self.seed = seed;
        self.pending = None;
        self.prepared.clear();
        self.attempts = 0;
        self.failure = None;
        self.notified = false;
    }

    /// Re-evaluate after navigation, queue edits, or repeat/shuffle changes.
    pub fn refresh(&mut self, queue: &QueueManager, paused: bool) {
        self.paused = paused;
        if !self.active {
            return;
        }
        if let Some(track) = queue.current_track() {
            if self.recent.back() != Some(&track.video_id) {
                self.recent.push_back(track.video_id.clone());
                if self.recent.len() > RECENT_LIMIT {
                    self.recent.pop_front();
                }
            }
        } else {
            self.active = false;
            self.waiting = false;
        }
        let seed = (self.active && queue.repeat_mode() == RepeatMode::Off && queue.is_last_track())
            .then(|| queue.current_track().unwrap().video_id.clone());
        if seed != self.seed {
            self.invalidate(seed);
        }
    }

    fn excluded(&self, queue: &QueueManager) -> HashSet<String> {
        queue
            .tracks()
            .into_iter()
            .map(|track| track.video_id)
            .chain(self.recent.iter().cloned())
            .collect()
    }

    /// Return at most one request per seed/attempt. A failed seed stays failed
    /// until the queue's end changes or the user explicitly starts playback.
    pub fn request(&mut self, queue: &QueueManager) -> Option<Request> {
        if self.pending.is_some() || !self.prepared.is_empty() || self.failure.is_some() {
            return None;
        }
        let seed = self.seed.clone()?;
        self.attempts += 1;
        let request = Request {
            generation: self.generation,
            seed,
            attempt: self.attempts,
            excluded: self.excluded(queue),
        };
        self.pending = Some(request.clone());
        Some(request)
    }

    pub fn pending(&self) -> Option<&Request> {
        self.pending.as_ref()
    }

    pub fn cancel_wait(&mut self) {
        self.waiting = false;
    }

    /// Filter again against the live queue: the user may have added tracks
    /// during HTTP. Empty/duplicate-only pages use the same bounded retry path.
    pub fn complete(
        &mut self,
        request: &Request,
        result: Result<Vec<Track>, Failure>,
        queue: &QueueManager,
    ) {
        if self.pending.as_ref() != Some(request) {
            return;
        }
        self.pending = None;
        let failure = match result {
            Ok(tracks) => {
                let mut excluded = self.excluded(queue);
                excluded.extend(request.excluded.iter().cloned());
                excluded.insert(request.seed.clone());
                self.prepared = tracks
                    .into_iter()
                    .filter(|track| {
                        !track.video_id.is_empty() && excluded.insert(track.video_id.clone())
                    })
                    .collect();
                if !self.prepared.is_empty() {
                    return;
                }
                Failure {
                    message: "Radio returned no new tracks".into(),
                    retryable: true,
                }
            }
            Err(failure) => failure,
        };
        if !failure.retryable || self.attempts >= MAX_ATTEMPTS {
            self.failure = Some(failure.message);
        }
    }

    /// Used both for natural EOF and explicit Next. Do not exhaust the queue
    /// while waiting: appending to an exhausted QueueManager selects the first
    /// addition, which would make an additional next_track skip that track.
    pub fn advance(&mut self, queue: &mut QueueManager) -> Advance {
        self.waiting = false;
        if queue.repeat_mode() == RepeatMode::Off && queue.is_last_track() {
            // Recheck at consumption time as well as HTTP completion.
            let mut excluded = self.excluded(queue);
            self.prepared
                .retain(|track| excluded.insert(track.video_id.clone()));
            if !self.prepared.is_empty() {
                queue.add_many(std::mem::take(&mut self.prepared));
            } else if self.active && self.seed.is_some() && self.failure.is_none() {
                self.waiting = true;
                return Advance::Wait;
            }
        }
        if queue.next_track().is_some() {
            Advance::Play
        } else {
            Advance::Idle
        }
    }

    /// A late batch can resume playback only if the user still wants playback.
    /// User-added tracks and a newly selected repeat mode take priority.
    pub fn ready(&mut self, queue: &mut QueueManager) -> Option<Advance> {
        if !self.waiting || self.paused {
            return None;
        }
        if !queue.is_last_track()
            || queue.repeat_mode() != RepeatMode::Off
            || !self.prepared.is_empty()
            || self.failure.is_some()
        {
            return Some(self.advance(queue));
        }
        None
    }

    pub fn take_notice(&mut self) -> Option<String> {
        if self.notified {
            return None;
        }
        let message = self.failure.as_ref()?;
        self.notified = true;
        Some(format!("Automatic radio unavailable: {message}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track::new_minimal(id, id, "Artist")
    }

    fn start(ids: &[&str]) -> (Autoplay, QueueManager) {
        let mut queue = QueueManager::new();
        queue.set_playlist(ids.iter().map(|id| track(id)).collect(), 0);
        let mut radio = Autoplay::default();
        radio.reset();
        radio.refresh(&queue, false);
        (radio, queue)
    }

    fn current(queue: &QueueManager) -> &str {
        &queue.current_track().unwrap().video_id
    }

    #[test]
    fn playlist_prefetches_only_on_final_track_and_preserves_order() {
        let (mut radio, mut queue) = start(&["a", "b", "c"]);
        assert!(radio.request(&queue).is_none());
        assert_eq!(radio.advance(&mut queue), Advance::Play);
        radio.refresh(&queue, false);
        assert_eq!(current(&queue), "b");
        assert!(radio.request(&queue).is_none());
        assert_eq!(radio.advance(&mut queue), Advance::Play);
        radio.refresh(&queue, false);
        let request = radio.request(&queue).unwrap();
        assert_eq!(request.seed, "c");
        assert!(radio.request(&queue).is_none(), "one fetch at a time");
        radio.complete(&request, Ok(vec![track("c"), track("d")]), &queue);
        assert_eq!(queue.tracks().len(), 3, "prefetch stays separate");
        assert_eq!(current(&queue), "c");
        assert_eq!(radio.advance(&mut queue), Advance::Play);
        assert_eq!(current(&queue), "d");
        assert_eq!(queue.tracks().len(), 4);
    }

    #[test]
    fn single_song_keeps_going_across_multiple_radio_batches() {
        let (mut radio, mut queue) = start(&["a"]);
        for (seed, next, last) in [("a", "b", "c"), ("c", "d", "e"), ("e", "f", "g")] {
            let request = radio.request(&queue).unwrap();
            assert_eq!(request.seed, seed);
            radio.complete(
                &request,
                Ok(vec![track(seed), track(next), track(last)]),
                &queue,
            );
            assert_eq!(radio.advance(&mut queue), Advance::Play);
            assert_eq!(current(&queue), next, "never skip the first addition");
            radio.refresh(&queue, false);
            assert!(radio.request(&queue).is_none());
            assert_eq!(radio.advance(&mut queue), Advance::Play);
            assert_eq!(current(&queue), last);
            radio.refresh(&queue, false);
        }
        assert_eq!(queue.tracks().len(), 7);
    }

    #[test]
    fn late_response_resumes_once_without_skipping_first_track() {
        let (mut radio, mut queue) = start(&["a"]);
        let request = radio.request(&queue).unwrap();
        assert_eq!(radio.advance(&mut queue), Advance::Wait);
        assert_eq!(radio.advance(&mut queue), Advance::Wait, "rapid Next");
        assert!(radio.request(&queue).is_none());
        assert!(radio.ready(&mut queue).is_none());
        radio.complete(&request, Ok(vec![track("b"), track("c")]), &queue);
        assert_eq!(radio.ready(&mut queue), Some(Advance::Play));
        assert_eq!(current(&queue), "b");
        assert!(radio.ready(&mut queue).is_none());
    }

    #[test]
    fn paused_wait_keeps_batch_until_explicit_resume() {
        let (mut radio, mut queue) = start(&["a"]);
        let request = radio.request(&queue).unwrap();
        assert_eq!(radio.advance(&mut queue), Advance::Wait);
        radio.refresh(&queue, true);
        radio.complete(&request, Ok(vec![track("b")]), &queue);
        assert!(radio.ready(&mut queue).is_none());
        assert_eq!(current(&queue), "a");
        radio.refresh(&queue, false);
        assert_eq!(radio.ready(&mut queue), Some(Advance::Play));
        assert_eq!(current(&queue), "b");
    }

    #[test]
    fn replacing_queue_rejects_old_completion_even_with_same_seed() {
        let (mut radio, mut queue) = start(&["a"]);
        let old = radio.request(&queue).unwrap();
        assert_eq!(radio.advance(&mut queue), Advance::Wait);
        queue.set_playlist(vec![track("a")], 0);
        radio.reset();
        radio.refresh(&queue, false);
        let new = radio.request(&queue).unwrap();
        radio.complete(&old, Ok(vec![track("wrong")]), &queue);
        assert_eq!(radio.pending(), Some(&new));
        assert!(radio.ready(&mut queue).is_none());
        radio.complete(&new, Ok(vec![track("right")]), &queue);
        assert_eq!(radio.advance(&mut queue), Advance::Play);
        assert_eq!(current(&queue), "right");
    }

    #[test]
    fn adding_tracks_while_waiting_prioritizes_them_and_invalidates_radio() {
        let (mut radio, mut queue) = start(&["a"]);
        let old = radio.request(&queue).unwrap();
        assert_eq!(radio.advance(&mut queue), Advance::Wait);
        queue.add_many(vec![track("user1"), track("user2")]);
        radio.refresh(&queue, false);
        radio.complete(&old, Ok(vec![track("radio")]), &queue);
        assert_eq!(radio.ready(&mut queue), Some(Advance::Play));
        assert_eq!(current(&queue), "user1");
        radio.refresh(&queue, false);
        assert!(radio.request(&queue).is_none());
        assert_eq!(radio.advance(&mut queue), Advance::Play);
        radio.refresh(&queue, false);
        assert_eq!(radio.request(&queue).unwrap().seed, "user2");
    }

    #[test]
    fn repeat_modes_suppress_fetches_and_keep_existing_loop_behavior() {
        for mode in [RepeatMode::One, RepeatMode::All] {
            let (mut radio, mut queue) = start(&["a", "b"]);
            queue.next_track();
            queue.set_repeat_mode(mode);
            radio.refresh(&queue, false);
            assert!(radio.request(&queue).is_none());
            assert_eq!(radio.advance(&mut queue), Advance::Play);
            assert_eq!(
                current(&queue),
                if mode == RepeatMode::One { "b" } else { "a" }
            );
            assert_eq!(queue.tracks().len(), 2);
        }
    }

    #[test]
    fn enabling_repeat_while_waiting_cancels_radio_and_replays_queue() {
        for mode in [RepeatMode::One, RepeatMode::All] {
            let (mut radio, mut queue) = start(&["a"]);
            let request = radio.request(&queue).unwrap();
            assert_eq!(radio.advance(&mut queue), Advance::Wait);
            queue.set_repeat_mode(mode);
            radio.refresh(&queue, false);
            radio.complete(&request, Ok(vec![track("b")]), &queue);
            assert_eq!(radio.ready(&mut queue), Some(Advance::Play));
            assert_eq!(current(&queue), "a");
            assert_eq!(queue.tracks().len(), 1);
            assert!(radio.request(&queue).is_none());
        }
    }

    #[test]
    fn turning_repeat_off_enables_radio() {
        let (mut radio, mut queue) = start(&["a"]);
        queue.set_repeat_mode(RepeatMode::One);
        radio.refresh(&queue, false);
        assert!(radio.request(&queue).is_none());
        queue.set_repeat_mode(RepeatMode::Off);
        radio.refresh(&queue, false);
        assert_eq!(radio.request(&queue).unwrap().seed, "a");
    }

    #[test]
    fn repeat_enabled_after_prefetch_does_not_extend_looped_queue() {
        let (mut radio, mut queue) = start(&["a"]);
        let request = radio.request(&queue).unwrap();
        radio.complete(&request, Ok(vec![track("b")]), &queue);
        queue.set_repeat_mode(RepeatMode::All);
        radio.refresh(&queue, false);
        assert_eq!(radio.advance(&mut queue), Advance::Play);
        assert_eq!(current(&queue), "a");
        assert_eq!(queue.tracks().len(), 1);
    }

    #[test]
    fn duplicates_and_recent_tracks_are_filtered_across_queue_replacements() {
        let (mut radio, mut queue) = start(&["recent"]);
        queue.set_playlist(vec![track("seed")], 0);
        radio.reset();
        radio.refresh(&queue, false);
        let request = radio.request(&queue).unwrap();
        radio.complete(
            &request,
            Ok(vec![
                track("seed"),
                track("recent"),
                track("b"),
                track("b"),
                track(""),
            ]),
            &queue,
        );
        assert_eq!(radio.advance(&mut queue), Advance::Play);
        assert_eq!(current(&queue), "b");
        assert_eq!(queue.tracks().len(), 2);
    }

    #[test]
    fn empty_batches_and_transient_errors_have_bounded_retries_and_one_notice() {
        let (mut radio, mut queue) = start(&["a"]);
        assert_eq!(radio.advance(&mut queue), Advance::Wait);
        for attempt in 1..=MAX_ATTEMPTS {
            let request = radio.request(&queue).unwrap();
            assert_eq!(request.attempt, attempt);
            let result = if attempt == 2 {
                Err(Failure {
                    message: "timeout".into(),
                    retryable: true,
                })
            } else {
                Ok(vec![track("a")])
            };
            radio.complete(&request, result, &queue);
            if attempt < MAX_ATTEMPTS {
                assert!(radio.take_notice().is_none());
                assert!(radio.ready(&mut queue).is_none());
            }
        }
        assert!(radio.request(&queue).is_none());
        assert_eq!(radio.ready(&mut queue), Some(Advance::Idle));
        assert!(radio.take_notice().unwrap().contains("no new tracks"));
        assert!(radio.take_notice().is_none());
        for _ in 0..5 {
            radio.refresh(&queue, false);
            assert!(
                radio.request(&queue).is_none(),
                "no retry loop after exhaustion"
            );
        }
    }

    #[test]
    fn authentication_failure_is_not_retried() {
        let (mut radio, mut queue) = start(&["a"]);
        let request = radio.request(&queue).unwrap();
        radio.complete(
            &request,
            Err(Failure {
                message: "Not signed in".into(),
                retryable: false,
            }),
            &queue,
        );
        assert!(radio.request(&queue).is_none());
        assert_eq!(radio.advance(&mut queue), Advance::Idle);
        assert!(radio.take_notice().unwrap().contains("Not signed in"));
        assert!(radio.take_notice().is_none());
    }

    #[test]
    fn clearing_queue_or_navigating_back_cannot_resume_pending_radio() {
        let (mut radio, mut queue) = start(&["a", "b"]);
        queue.next_track();
        radio.refresh(&queue, false);
        let request = radio.request(&queue).unwrap();
        assert_eq!(radio.advance(&mut queue), Advance::Wait);
        radio.cancel_wait();
        queue.previous_track();
        radio.refresh(&queue, false);
        radio.complete(&request, Ok(vec![track("wrong")]), &queue);
        assert!(radio.ready(&mut queue).is_none());
        assert_eq!(current(&queue), "a");
        queue.clear();
        radio.refresh(&queue, false);
        assert!(radio.request(&queue).is_none());
        assert!(radio.ready(&mut queue).is_none());
    }

    #[test]
    fn shuffle_toggle_preserves_appended_recommendations_and_selection() {
        let (mut radio, mut queue) = start(&["a"]);
        queue.toggle_shuffle();
        let request = radio.request(&queue).unwrap();
        radio.complete(&request, Ok(vec![track("b"), track("c")]), &queue);
        assert_eq!(radio.advance(&mut queue), Advance::Play);
        assert_eq!(current(&queue), "b");
        queue.toggle_shuffle();
        assert_eq!(current(&queue), "b");
        assert_eq!(
            queue
                .tracks()
                .iter()
                .map(|track| track.video_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn recent_history_stays_bounded_during_long_sessions() {
        let (mut radio, mut queue) = start(&["first"]);
        for index in 0..RECENT_LIMIT + 5 {
            queue.set_playlist(vec![track(&index.to_string())], 0);
            radio.reset();
            radio.refresh(&queue, false);
        }
        assert_eq!(radio.recent.len(), RECENT_LIMIT);
        let request = radio.request(&queue).unwrap();
        assert!(!request.excluded.contains("first"));
        assert!(request.excluded.contains("104"));
    }
}
