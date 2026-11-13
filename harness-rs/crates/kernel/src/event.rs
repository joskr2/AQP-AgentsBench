use tokio::sync::broadcast;

/// Events published by the agent loop and consumed by any number of observers.
///
/// Keeping this enum small is deliberate: it is the stable seam between the
/// runtime and presentation layers (CLI, future protocol adapter, telemetry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessEvent {
    SessionOpened {
        path: String,
    },
    TurnStarted,
    StepStarted {
        step: usize,
    },
    TextDelta {
        text: String,
    },
    ToolCallRequested {
        name: String,
    },
    ToolCallFinished {
        name: String,
        ok: bool,
    },
    TurnFinished {
        stop_reason: String,
    },
    Cancelled,
}

/// Fan-out event bus backed by a bounded broadcast channel.
///
/// `emit` never blocks and never fails: when there are no subscribers the
/// event is dropped (that is the normal case for headless runs).
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<HarnessEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HarnessEvent> {
        self.sender.subscribe()
    }

    pub fn emit(&self, event: HarnessEvent) {
        let _ = self.sender.send(event);
    }

    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_reaches_subscribers() {
        let bus = EventBus::new(8);
        let mut receiver = bus.subscribe();
        bus.emit(HarnessEvent::TurnStarted);
        assert_eq!(receiver.try_recv().unwrap(), HarnessEvent::TurnStarted);
        assert_eq!(bus.subscriber_count(), 1);
    }

    #[test]
    fn emit_without_subscribers_is_a_no_op() {
        let bus = EventBus::new(8);
        bus.emit(HarnessEvent::Cancelled);
        assert_eq!(bus.subscriber_count(), 0);
    }
}
