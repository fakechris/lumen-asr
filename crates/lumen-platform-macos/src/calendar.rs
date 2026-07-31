//! macOS calendar (EventKit) lookup for linking a just-started meeting
//! recording to the calendar event it belongs to.
//!
//! One read-only, single-shot query: "which event is the user in (or about to
//! join) right now?". The app layer uses the answer to auto-title an untitled
//! meeting and to note the attendee names. Nothing is ever written to the
//! calendar.
//!
//! ## Binding style
//! EventKit is bound by hand with `objc2::msg_send` on runtime-looked-up
//! classes (the same style as the `CATapDescription` use in `system_audio`),
//! so no extra crate dependency is needed and every call degrades to `None`
//! when a class/selector is missing.
//!
//! ## Permission (TCC)
//! Reading events requires user consent
//! (`NSCalendarsFullAccessUsageDescription` on macOS 14+,
//! `NSCalendarsUsageDescription` before). [`request_access`] uses the
//! macOS 14+ `requestFullAccessToEventsWithCompletion:` when the selector
//! exists and falls back to the legacy `requestAccessToEntityType:completion:`
//! otherwise. A denied/restricted permission, a missing usage string, or a
//! completion that never arrives (bounded wait) all yield `false`/`None` —
//! the lookup **never blocks or fails the recording**.
//!
//! ## Verification note
//! A CI/sandbox host has no calendar database or TCC session, so the live
//! query needs on-device validation. The window-selection logic is pure and
//! unit-tested cross-platform; the FFI layer is compile-checked everywhere.

use std::time::Duration;

/// How far back the query window reaches, so an event that started a few
/// minutes ago (the user joined late) still matches as "ongoing".
pub const CALENDAR_LOOKBACK_MINUTES: u32 = 5;

/// How long to wait for the authorization completion. An access request is
/// only ever issued when the status is still *not determined*, i.e. the
/// system permission prompt just appeared. The bounded wait means the very
/// first recording usually gets no link (the user is still reading the
/// prompt) — once answered, every later recording resolves instantly. On
/// timeout the lookup simply reports "no access" for this session.
const ACCESS_WAIT: Duration = Duration::from_secs(2);

/// A calendar event matched at recording start.
#[derive(Debug, Clone, PartialEq)]
pub struct CalendarEventInfo {
    /// Event title (non-empty; empty-titled events are never selected).
    pub title: String,
    /// Event start, seconds since the Unix epoch.
    pub start_epoch_seconds: f64,
    /// Event end, seconds since the Unix epoch.
    pub end_epoch_seconds: f64,
    /// Attendee display labels: `姓名`, `姓名 <email>`, or the bare email
    /// when the participant has no name. Deduplicated, order preserved.
    pub attendee_names: Vec<String>,
}

/// Time-window facts about one fetched event, for the pure selection step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalendarCandidate {
    /// Seconds since the Unix epoch.
    pub start_epoch_seconds: f64,
    /// Seconds since the Unix epoch.
    pub end_epoch_seconds: f64,
    /// All-day events are calendar decorations (birthdays, holidays), not
    /// meetings — never selected.
    pub all_day: bool,
}

/// Pick the event a recording started "for": among `candidates`, prefer the
/// non-all-day event **ongoing** at `now` (most recently started wins when
/// several overlap); otherwise the one **starting soonest** within the next
/// `lookahead_minutes`. `None` when nothing qualifies.
pub fn select_event_in_window(
    now_epoch_seconds: f64,
    lookahead_minutes: u32,
    candidates: &[CalendarCandidate],
) -> Option<usize> {
    let now = now_epoch_seconds;
    let horizon = now + f64::from(lookahead_minutes) * 60.0;

    // Ongoing: started already, not yet over. Most recently started wins —
    // with back-to-back overlapping events, the newer one is the meeting the
    // user just joined.
    let ongoing = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.all_day && c.start_epoch_seconds <= now && c.end_epoch_seconds > now)
        .max_by(|(_, a), (_, b)| a.start_epoch_seconds.total_cmp(&b.start_epoch_seconds));
    if let Some((index, _)) = ongoing {
        return Some(index);
    }

    // Upcoming: starts within the look-ahead window. Earliest start wins.
    candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            !c.all_day && c.start_epoch_seconds > now && c.start_epoch_seconds <= horizon
        })
        .min_by(|(_, a), (_, b)| a.start_epoch_seconds.total_cmp(&b.start_epoch_seconds))
        .map(|(index, _)| index)
}

/// Ensure calendar read access, prompting the user on first use. `true` only
/// when read access is (now) granted. Never blocks longer than the bounded
/// waits above; `false` on non-macOS, on denial, and on timeout.
pub fn request_access() -> bool {
    #[cfg(target_os = "macos")]
    {
        imp::request_access()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Query the calendar once for the event the user is currently in, or the
/// nearest one starting within `lookahead_minutes` (see
/// [`select_event_in_window`]). Requests access first (prompting on first
/// use). `None` on non-macOS, without permission, and when no event matches —
/// never an error.
pub fn current_or_upcoming_event(lookahead_minutes: u32) -> Option<CalendarEventInfo> {
    #[cfg(target_os = "macos")]
    {
        imp::current_or_upcoming_event(lookahead_minutes)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = lookahead_minutes;
        None
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{
        select_event_in_window, CalendarCandidate, CalendarEventInfo, ACCESS_WAIT,
        CALENDAR_LOOKBACK_MINUTES,
    };
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::sel;
    use objc2_foundation::{NSArray, NSString};

    // Force EventKit.framework to load so `EKEventStore` is registered with
    // the runtime; no C symbols are imported.
    #[link(name = "EventKit", kind = "framework")]
    extern "C" {}

    /// `EKEntityTypeEvent`.
    const ENTITY_TYPE_EVENT: usize = 0;
    /// `EKAuthorizationStatus`: not-determined / restricted / denied /
    /// full-access (== legacy authorized) / write-only.
    const STATUS_NOT_DETERMINED: isize = 0;
    const STATUS_FULL_ACCESS: isize = 3;

    /// Upper bound on attendee labels forwarded to the notes line; a huge
    /// company-wide invite should not flood the meeting notes.
    const MAX_ATTENDEES: usize = 30;

    fn event_store_class() -> Option<&'static AnyClass> {
        AnyClass::get(c"EKEventStore")
    }

    fn new_event_store(class: &AnyClass) -> Option<Retained<AnyObject>> {
        // SAFETY: plain alloc/init of a runtime-verified class; init consumes
        // the alloc and `from_raw` takes ownership of the +1 result.
        unsafe {
            let allocated: *mut AnyObject = msg_send![class, alloc];
            let initialized: *mut AnyObject = msg_send![allocated, init];
            Retained::from_raw(initialized)
        }
    }

    fn authorization_status(class: &AnyClass) -> isize {
        // SAFETY: documented EKEventStore class method taking EKEntityType.
        unsafe { msg_send![class, authorizationStatusForEntityType: ENTITY_TYPE_EVENT] }
    }

    pub(super) fn request_access() -> bool {
        let Some(class) = event_store_class() else {
            return false;
        };
        let status = authorization_status(class);
        if status == STATUS_FULL_ACCESS {
            return true;
        }
        // Restricted / denied / write-only: nothing to ask; stay silent.
        if status != STATUS_NOT_DETERMINED {
            return false;
        }
        let Some(store) = new_event_store(class) else {
            return false;
        };

        // The completion may fire on any queue; hand the answer back over a
        // channel. The sender lives in a Mutex<Option<…>> so the block stays
        // a plain `Fn` and the send happens at most once.
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let tx = Mutex::new(Some(tx));
        let completion = RcBlock::new(move |granted: Bool, _error: *mut AnyObject| {
            if let Ok(mut slot) = tx.lock() {
                if let Some(tx) = slot.take() {
                    let _ = tx.send(granted.as_bool());
                }
            }
        });

        // macOS 14+ API when present; legacy request otherwise.
        let full_access_selector = sel!(requestFullAccessToEventsWithCompletion:);
        // SAFETY: respondsToSelector: on a live object; both request calls
        // pass a retained completion block Core Foundation copies.
        unsafe {
            let has_full_access_api: bool =
                msg_send![&*store, respondsToSelector: full_access_selector];
            if has_full_access_api {
                let _: () =
                    msg_send![&*store, requestFullAccessToEventsWithCompletion: &*completion];
            } else {
                let _: () = msg_send![
                    &*store,
                    requestAccessToEntityType: ENTITY_TYPE_EVENT,
                    completion: &*completion
                ];
            }
        }
        // Bounded wait: timeout (user still reading the prompt) or a hung
        // callback simply reports "no access" for this session.
        rx.recv_timeout(ACCESS_WAIT).unwrap_or(false)
    }

    /// `[NSDate dateWithTimeIntervalSinceNow:offset]` as an untyped object.
    fn date_since_now(offset_seconds: f64) -> Option<Retained<AnyObject>> {
        let class = AnyClass::get(c"NSDate")?;
        // SAFETY: documented NSDate class constructor.
        unsafe { msg_send![class, dateWithTimeIntervalSinceNow: offset_seconds] }
    }

    fn epoch_seconds(date: Option<Retained<AnyObject>>) -> Option<f64> {
        let date = date?;
        // SAFETY: timeIntervalSince1970 is a documented NSDate property.
        let seconds: f64 = unsafe { msg_send![&*date, timeIntervalSince1970] };
        Some(seconds)
    }

    /// Attendee display labels for one event: name, `name <email>`, or the
    /// bare email. Empty when the event has no attendees.
    fn attendee_labels(event: &AnyObject) -> Vec<String> {
        // SAFETY: `attendees` is a documented, nullable EKCalendarItem
        // property (NSArray<EKParticipant>).
        let attendees: Option<Retained<NSArray<AnyObject>>> =
            unsafe { msg_send![event, attendees] };
        let Some(attendees) = attendees else {
            return Vec::new();
        };
        let mut labels: Vec<String> = Vec::new();
        for index in 0..attendees.count() {
            if labels.len() >= MAX_ATTENDEES {
                break;
            }
            let participant = attendees.objectAtIndex(index);
            // SAFETY: `name` is a documented, nullable EKParticipant property.
            let name: Option<Retained<NSString>> = unsafe { msg_send![&*participant, name] };
            let name = name.map(|n| n.to_string()).unwrap_or_default();
            let name = name.trim().to_string();
            let email = participant_email(&participant);
            let label = if name.is_empty() {
                match email {
                    Some(email) => email,
                    None => continue,
                }
            } else if let Some(email) = email.filter(|e| !e.eq_ignore_ascii_case(&name)) {
                format!("{name} <{email}>")
            } else {
                name
            };
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        labels
    }

    /// The participant's email, when its URL is a `mailto:`.
    fn participant_email(participant: &AnyObject) -> Option<String> {
        // SAFETY: `URL` (NSURL) and `absoluteString` are documented
        // properties.
        let url: Option<Retained<AnyObject>> = unsafe { msg_send![participant, URL] };
        let url = url?;
        let absolute: Option<Retained<NSString>> = unsafe { msg_send![&*url, absoluteString] };
        absolute?
            .to_string()
            .strip_prefix("mailto:")
            .map(|email| email.trim().to_string())
            .filter(|email| !email.is_empty())
    }

    pub(super) fn current_or_upcoming_event(lookahead_minutes: u32) -> Option<CalendarEventInfo> {
        if !request_access() {
            return None;
        }
        let class = event_store_class()?;
        let store = new_event_store(class)?;

        let lookback = f64::from(CALENDAR_LOOKBACK_MINUTES) * 60.0;
        let lookahead = f64::from(lookahead_minutes) * 60.0;
        let range_start = date_since_now(-lookback)?;
        let range_end = date_since_now(lookahead)?;
        // SAFETY: documented EKEventStore predicate + synchronous fetch; a
        // nil calendars argument means "all calendars". The fetch runs on
        // this (background) thread and the events are consumed here only.
        let predicate: Option<Retained<AnyObject>> = unsafe {
            msg_send![
                &*store,
                predicateForEventsWithStartDate: &*range_start,
                endDate: &*range_end,
                calendars: std::ptr::null_mut::<AnyObject>()
            ]
        };
        let predicate = predicate?;
        let events: Option<Retained<NSArray<AnyObject>>> =
            unsafe { msg_send![&*store, eventsMatchingPredicate: &*predicate] };
        let events = events?;

        let mut candidates: Vec<CalendarCandidate> = Vec::new();
        let mut fetched: Vec<(Retained<AnyObject>, String, f64, f64)> = Vec::new();
        for index in 0..events.count() {
            let event = events.objectAtIndex(index);
            // SAFETY: title / isAllDay / startDate / endDate are documented
            // EKEvent (EKCalendarItem) properties.
            let title: Option<Retained<NSString>> = unsafe { msg_send![&*event, title] };
            let title = title.map(|t| t.to_string()).unwrap_or_default();
            let title = title.trim().to_string();
            if title.is_empty() {
                continue; // useless for naming; skip entirely.
            }
            let all_day: bool = unsafe { msg_send![&*event, isAllDay] };
            let start = epoch_seconds(unsafe { msg_send![&*event, startDate] });
            let end = epoch_seconds(unsafe { msg_send![&*event, endDate] });
            let (Some(start), Some(end)) = (start, end) else {
                continue;
            };
            candidates.push(CalendarCandidate {
                start_epoch_seconds: start,
                end_epoch_seconds: end,
                all_day,
            });
            fetched.push((event, title, start, end));
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs_f64();
        let selected = select_event_in_window(now, lookahead_minutes, &candidates)?;
        let (event, title, start, end) = &fetched[selected];
        Some(CalendarEventInfo {
            title: title.clone(),
            start_epoch_seconds: *start,
            end_epoch_seconds: *end,
            attendee_names: attendee_labels(event),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(start: f64, end: f64) -> CalendarCandidate {
        CalendarCandidate {
            start_epoch_seconds: start,
            end_epoch_seconds: end,
            all_day: false,
        }
    }

    const NOW: f64 = 10_000.0;

    #[test]
    fn no_candidates_selects_nothing() {
        assert_eq!(select_event_in_window(NOW, 15, &[]), None);
    }

    #[test]
    fn ongoing_event_is_selected() {
        let events = [candidate(NOW - 600.0, NOW + 600.0)];
        assert_eq!(select_event_in_window(NOW, 15, &events), Some(0));
    }

    #[test]
    fn most_recently_started_ongoing_wins() {
        let events = [
            candidate(NOW - 3_600.0, NOW + 3_600.0), // long-running block
            candidate(NOW - 60.0, NOW + 1_800.0),    // just joined this one
        ];
        assert_eq!(select_event_in_window(NOW, 15, &events), Some(1));
    }

    #[test]
    fn ongoing_beats_upcoming() {
        let events = [
            candidate(NOW + 300.0, NOW + 3_600.0), // starts in 5 min
            candidate(NOW - 300.0, NOW + 1_800.0), // already running
        ];
        assert_eq!(select_event_in_window(NOW, 15, &events), Some(1));
    }

    #[test]
    fn earliest_upcoming_within_window_wins() {
        let events = [
            candidate(NOW + 840.0, NOW + 4_000.0),
            candidate(NOW + 300.0, NOW + 3_600.0),
        ];
        assert_eq!(select_event_in_window(NOW, 15, &events), Some(1));
    }

    #[test]
    fn upcoming_outside_window_is_ignored() {
        let events = [candidate(NOW + 16.0 * 60.0, NOW + 3_600.0)];
        assert_eq!(select_event_in_window(NOW, 15, &events), None);
    }

    #[test]
    fn already_ended_event_is_ignored() {
        let events = [candidate(NOW - 3_600.0, NOW - 60.0)];
        assert_eq!(select_event_in_window(NOW, 15, &events), None);
    }

    #[test]
    fn all_day_events_are_ignored() {
        let events = [CalendarCandidate {
            start_epoch_seconds: NOW - 3_600.0,
            end_epoch_seconds: NOW + 80_000.0,
            all_day: true,
        }];
        assert_eq!(select_event_in_window(NOW, 15, &events), None);
    }

    #[test]
    fn boundary_start_exactly_at_window_edge_is_included() {
        let events = [candidate(NOW + 15.0 * 60.0, NOW + 3_600.0)];
        assert_eq!(select_event_in_window(NOW, 15, &events), Some(0));
    }

    #[test]
    fn non_macos_stubs_return_nothing() {
        // On macOS these exercise the real gate (harmless: status check or a
        // permission-less None); elsewhere they are the constant stubs.
        if cfg!(not(target_os = "macos")) {
            assert!(!request_access());
            assert!(current_or_upcoming_event(15).is_none());
        }
    }
}
