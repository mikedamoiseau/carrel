//! Typed lifecycle event bus (plugin/hook system M1 — see
//! `docs/superpowers/specs/2026-06-12-plugin-hook-system-design.md` §4.1).
//!
//! Payloads carry IDs, not full records — consumers fetch details through
//! permission-gated APIs, keeping the event surface permission-neutral.
//! `EventBus::emit` is fire-and-forget: it enqueues onto a bounded channel
//! consumed by one dedicated dispatch thread, so a slow listener can never
//! block the emitting command. When the queue is full the event is dropped
//! for listeners (counted and logged), never blocking the emitter.

use crate::models::BookFormat;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

/// Where an imported book came from. Granularity matches what the backend
/// can actually distinguish: the `import_book` IPC serves both the file
/// picker and drag-and-drop (`Manual`), and `download_opds_book` serves both
/// catalog downloads and direct-URL imports (`Download`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSource {
    Manual,
    FolderScan,
    Download,
}

/// Direction of a completed sync operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncDirection {
    Pull,
    Push,
}

/// A lifecycle event. One variant per hook point (spec §4.1).
#[derive(Debug, Clone, PartialEq)]
pub enum CarrelEvent {
    AppStarted,
    BookImported {
        book_id: String,
        format: BookFormat,
        source: ImportSource,
    },
    BookOpened {
        book_id: String,
    },
    BookClosed {
        book_id: String,
    },
    BookFinished {
        book_id: String,
    },
    HighlightCreated {
        book_id: String,
        highlight_id: String,
    },
    // Update/delete commands only receive the highlight id; the owning
    // book_id becomes resolvable through the M2 host API getters.
    HighlightUpdated {
        highlight_id: String,
    },
    HighlightDeleted {
        highlight_id: String,
    },
    BookmarkCreated {
        book_id: String,
        bookmark_id: String,
    },
    MetadataEnriched {
        book_id: String,
        provider: String,
    },
    BackupCompleted {
        provider: String,
        success: bool,
    },
    SyncCompleted {
        direction: SyncDirection,
        success: bool,
    },
}

impl CarrelEvent {
    /// Stable event names — the contract for plugin manifests
    /// (`[events] subscribe = [...]`). Keep in sync with `ALL_NAMES`.
    pub fn name(&self) -> &'static str {
        match self {
            CarrelEvent::AppStarted => "AppStarted",
            CarrelEvent::BookImported { .. } => "BookImported",
            CarrelEvent::BookOpened { .. } => "BookOpened",
            CarrelEvent::BookClosed { .. } => "BookClosed",
            CarrelEvent::BookFinished { .. } => "BookFinished",
            CarrelEvent::HighlightCreated { .. } => "HighlightCreated",
            CarrelEvent::HighlightUpdated { .. } => "HighlightUpdated",
            CarrelEvent::HighlightDeleted { .. } => "HighlightDeleted",
            CarrelEvent::BookmarkCreated { .. } => "BookmarkCreated",
            CarrelEvent::MetadataEnriched { .. } => "MetadataEnriched",
            CarrelEvent::BackupCompleted { .. } => "BackupCompleted",
            CarrelEvent::SyncCompleted { .. } => "SyncCompleted",
        }
    }

    /// Every valid event name, for manifest validation.
    pub const ALL_NAMES: [&'static str; 12] = [
        "AppStarted",
        "BookImported",
        "BookOpened",
        "BookClosed",
        "BookFinished",
        "HighlightCreated",
        "HighlightUpdated",
        "HighlightDeleted",
        "BookmarkCreated",
        "MetadataEnriched",
        "BackupCompleted",
        "SyncCompleted",
    ];
}

/// A registered event listener. Dispatched sequentially on the bus's own
/// thread; implementations must be cheap or do their own offloading.
pub trait EventListener: Send {
    fn on_event(&mut self, event: &CarrelEvent);
}

/// Blanket impl so closures can subscribe without a named type.
impl<F: FnMut(&CarrelEvent) + Send> EventListener for F {
    fn on_event(&mut self, event: &CarrelEvent) {
        self(event)
    }
}

/// Fire-and-forget event bus with a bounded queue and a single dispatch
/// thread.
pub struct EventBus {
    sender: SyncSender<CarrelEvent>,
    listeners: Arc<Mutex<Vec<Box<dyn EventListener>>>>,
    dropped: AtomicU64,
}

impl EventBus {
    /// Queue capacity. Overflow drops the event for listeners (logged).
    pub const QUEUE_CAPACITY: usize = 256;

    pub fn new() -> Self {
        let (sender, receiver) = sync_channel::<CarrelEvent>(Self::QUEUE_CAPACITY);
        let listeners: Arc<Mutex<Vec<Box<dyn EventListener>>>> = Arc::new(Mutex::new(Vec::new()));

        let thread_listeners = Arc::clone(&listeners);
        // The thread exits when every sender is dropped (recv() errs), so a
        // test-local bus tears down with the bus itself.
        thread::Builder::new()
            .name("carrel-events".into())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    let mut listeners = thread_listeners
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    for listener in listeners.iter_mut() {
                        // A panicking listener must not kill the dispatch
                        // thread or starve the remaining listeners.
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            listener.on_event(&event)
                        }));
                        if result.is_err() {
                            tracing::error!(?event, "event listener panicked during dispatch");
                        }
                    }
                }
            })
            .expect("failed to spawn carrel-events dispatch thread");

        Self {
            sender,
            listeners,
            dropped: AtomicU64::new(0),
        }
    }

    /// Register a listener. Listeners receive events in emission order.
    pub fn subscribe(&self, listener: Box<dyn EventListener>) {
        self.listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(listener);
    }

    /// Enqueue an event for dispatch. Never blocks; drops on overflow.
    pub fn emit(&self, event: CarrelEvent) {
        match self.sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(event)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(?event, "event queue full — dropping event for listeners");
            }
            Err(TrySendError::Disconnected(event)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                tracing::error!(?event, "event dispatch thread gone — dropping event");
            }
        }
    }

    /// Number of events dropped due to queue overflow since creation.
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide bus instance. Inert (no thread) until first accessed.
pub fn bus() -> &'static EventBus {
    static BUS: OnceLock<EventBus> = OnceLock::new();
    BUS.get_or_init(EventBus::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    const RECV_TIMEOUT: Duration = Duration::from_secs(2);

    fn channel_listener() -> (Box<dyn EventListener>, mpsc::Receiver<CarrelEvent>) {
        let (tx, rx) = mpsc::channel();
        let listener = move |event: &CarrelEvent| {
            let _ = tx.send(event.clone());
        };
        (Box::new(listener), rx)
    }

    #[test]
    fn emit_delivers_event_to_subscriber() {
        let bus = EventBus::new();
        let (listener, rx) = channel_listener();
        bus.subscribe(listener);

        bus.emit(CarrelEvent::BookImported {
            book_id: "b1".into(),
            format: BookFormat::Epub,
            source: ImportSource::Manual,
        });

        let received = rx.recv_timeout(RECV_TIMEOUT).expect("event not delivered");
        assert_eq!(
            received,
            CarrelEvent::BookImported {
                book_id: "b1".into(),
                format: BookFormat::Epub,
                source: ImportSource::Manual,
            }
        );
    }

    #[test]
    fn all_subscribers_receive_each_event() {
        let bus = EventBus::new();
        let (l1, rx1) = channel_listener();
        let (l2, rx2) = channel_listener();
        bus.subscribe(l1);
        bus.subscribe(l2);

        bus.emit(CarrelEvent::AppStarted);

        assert_eq!(
            rx1.recv_timeout(RECV_TIMEOUT).unwrap(),
            CarrelEvent::AppStarted
        );
        assert_eq!(
            rx2.recv_timeout(RECV_TIMEOUT).unwrap(),
            CarrelEvent::AppStarted
        );
    }

    #[test]
    fn events_arrive_in_emission_order() {
        let bus = EventBus::new();
        let (listener, rx) = channel_listener();
        bus.subscribe(listener);

        for i in 0..10 {
            bus.emit(CarrelEvent::BookOpened {
                book_id: format!("b{i}"),
            });
        }

        for i in 0..10 {
            let event = rx.recv_timeout(RECV_TIMEOUT).unwrap();
            assert_eq!(
                event,
                CarrelEvent::BookOpened {
                    book_id: format!("b{i}"),
                }
            );
        }
    }

    #[test]
    fn emit_without_subscribers_does_not_panic_or_block() {
        let bus = EventBus::new();
        bus.emit(CarrelEvent::BookFinished {
            book_id: "b1".into(),
        });
        assert_eq!(bus.dropped_events(), 0);
    }

    #[test]
    fn emit_never_blocks_when_queue_overflows() {
        let bus = EventBus::new();
        // Listener that blocks until released, jamming the dispatch thread
        // so the queue fills up.
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate_rx);
        bus.subscribe(Box::new(move |_: &CarrelEvent| {
            let _ = gate.lock().unwrap().recv_timeout(RECV_TIMEOUT);
        }));

        // One event occupies the dispatcher, QUEUE_CAPACITY fill the queue,
        // the rest must be dropped — and emit() must return regardless.
        let total = EventBus::QUEUE_CAPACITY + 10;
        let start = std::time::Instant::now();
        for _ in 0..=total {
            bus.emit(CarrelEvent::AppStarted);
        }
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "emit() blocked on a full queue"
        );
        assert!(bus.dropped_events() > 0, "overflow was not counted");

        // Release the dispatcher so the test tears down cleanly.
        for _ in 0..=total {
            let _ = gate_tx.send(());
        }
    }

    #[test]
    fn panicking_listener_does_not_kill_dispatch_or_other_listeners() {
        let bus = EventBus::new();
        bus.subscribe(Box::new(|_: &CarrelEvent| {
            panic!("listener blew up");
        }));
        let (listener, rx) = channel_listener();
        bus.subscribe(listener);

        bus.emit(CarrelEvent::AppStarted);
        bus.emit(CarrelEvent::BookFinished {
            book_id: "b1".into(),
        });

        // The healthy listener still receives both events, in order, even
        // though the first listener panics on every dispatch.
        assert_eq!(
            rx.recv_timeout(RECV_TIMEOUT).unwrap(),
            CarrelEvent::AppStarted
        );
        assert_eq!(
            rx.recv_timeout(RECV_TIMEOUT).unwrap(),
            CarrelEvent::BookFinished {
                book_id: "b1".into(),
            }
        );
    }

    #[test]
    fn event_names_cover_every_variant_and_match_all_names() {
        let samples: Vec<CarrelEvent> = vec![
            CarrelEvent::AppStarted,
            CarrelEvent::BookImported {
                book_id: "b".into(),
                format: BookFormat::Epub,
                source: ImportSource::Manual,
            },
            CarrelEvent::BookOpened {
                book_id: "b".into(),
            },
            CarrelEvent::BookClosed {
                book_id: "b".into(),
            },
            CarrelEvent::BookFinished {
                book_id: "b".into(),
            },
            CarrelEvent::HighlightCreated {
                book_id: "b".into(),
                highlight_id: "h".into(),
            },
            CarrelEvent::HighlightUpdated {
                highlight_id: "h".into(),
            },
            CarrelEvent::HighlightDeleted {
                highlight_id: "h".into(),
            },
            CarrelEvent::BookmarkCreated {
                book_id: "b".into(),
                bookmark_id: "m".into(),
            },
            CarrelEvent::MetadataEnriched {
                book_id: "b".into(),
                provider: "p".into(),
            },
            CarrelEvent::BackupCompleted {
                provider: "p".into(),
                success: true,
            },
            CarrelEvent::SyncCompleted {
                direction: SyncDirection::Pull,
                success: true,
            },
        ];
        let names: Vec<&str> = samples.iter().map(|e| e.name()).collect();
        assert_eq!(names, CarrelEvent::ALL_NAMES.to_vec());
    }

    #[test]
    fn global_bus_returns_same_instance() {
        let a = bus() as *const EventBus;
        let b = bus() as *const EventBus;
        assert_eq!(a, b);
    }
}
