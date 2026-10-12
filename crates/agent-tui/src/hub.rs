//! The bridge between a run and the screen, in both directions.
//!
//! The loop already speaks two traits to whoever is watching: [`Observer`] for
//! what happened while it happened, and [`Approver`] for the questions it must
//! stop for. A terminal run answers those from `eprintln!` and a line read from
//! stdin; a full-screen run has to answer them from a drawing loop that owns the
//! keyboard. This is the adapter that lets the same loop do both: it implements
//! the two traits and does nothing but hand the message to the screen over a
//! channel.
//!
//! Both directions matter for safety, so each has a defined answer for the case
//! where the screen is not listening.
//!
//! - An **event** whose channel is closed is dropped. The session records every
//!   event whether or not anyone renders it, so the transcript on disk stays
//!   complete; only the live picture is lost.
//! - An **approval** whose reply cannot be delivered, or whose question the
//!   screen closes without answering, is `Rejected`. A run must not read an
//!   absent answer as consent — the same rule `agent-cli`'s terminal approver
//!   follows when there is no TTY to ask on.

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use agent_core::{AgentEvent, ApprovalDecision, ApprovalRequest, Approver, Observer};

/// One thing the screen needs to know.
#[derive(Debug)]
pub enum Msg {
    /// A step of the run, for the transcript, the log, the plan or the diff.
    Event(AgentEvent),
    /// A question the loop is stopped on until a key answers it.
    Approval {
        request: ApprovalRequest,
        reply: oneshot::Sender<ApprovalDecision>,
    },
}

/// The [`Observer`] and [`Approver`] a TUI run is given.
///
/// Build one with [`Hub::new`], take the receiver with [`Hub::feed`] before the
/// hub is shared, then hand the hub to `Agent::with_observer` and
/// `Agent::with_approver`. The receiver comes out separately because a screen
/// loop must own it to read from it, while the loop hands out `Arc<Hub>`.
pub struct Hub {
    tx: mpsc::UnboundedSender<Msg>,
    rx: Option<mpsc::UnboundedReceiver<Msg>>,
}

impl Hub {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { tx, rx: Some(rx) }
    }

    /// The end the screen reads. `None` the second time it is asked for, because
    /// a channel has one reader by construction.
    pub fn feed(&mut self) -> Option<mpsc::UnboundedReceiver<Msg>> {
        self.rx.take()
    }

    fn send(&self, msg: Msg) -> bool {
        self.tx.send(msg).is_ok()
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer for Hub {
    fn observe(&self, event: &AgentEvent) {
        if !self.send(Msg::Event(event.clone())) {
            // The screen has gone. Say so in the log rather than losing the fact
            // that a run kept going with nobody watching it.
            tracing::debug!("the screen closed; this event is in the session only");
        }
    }
}

#[async_trait]
impl Approver for Hub {
    async fn approve(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let (reply, answer) = oneshot::channel();
        if !self.send(Msg::Approval {
            request: request.clone(),
            reply,
        }) {
            return ApprovalDecision::Rejected {
                reason: "the screen was gone when the question was asked, and an agent that \
                        cannot show a person what it wants to do does not do it"
                    .to_owned(),
            };
        }
        match answer.await {
            Ok(decision) => decision,
            // The question was on screen and the screen closed before a key
            // answered it. Refusing is the only answer that keeps that honest.
            Err(_) => ApprovalDecision::Rejected {
                reason: "the question was shown and no key answered it".to_owned(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_core::Permission;

    use super::*;

    /// A hub with the screen's end of the channel taken out, which is how the
    /// CLI builds one: the hub goes to the loop, the feed stays with the screen.
    fn pair() -> (Arc<Hub>, mpsc::UnboundedReceiver<Msg>) {
        let mut hub = Hub::new();
        let feed = hub.feed().expect("the feed has not been taken yet");
        (Arc::new(hub), feed)
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest::new(Permission::Write, "write notes.md", "+a line")
    }

    fn event() -> AgentEvent {
        AgentEvent::Warning {
            message: "context is 90% full".to_owned(),
        }
    }

    #[tokio::test]
    async fn an_event_reaches_the_screen_with_its_content_intact() {
        let (hub, mut feed) = pair();
        hub.observe(&event());
        match feed.recv().await {
            Some(Msg::Event(AgentEvent::Warning { message })) => {
                assert_eq!(message, "context is 90% full");
            }
            other => panic!("the event did not arrive as sent: {other:?}"),
        }
    }

    #[test]
    fn the_feed_is_handed_over_once() {
        let mut hub = Hub::new();
        assert!(
            hub.feed().is_some(),
            "the screen reads the channel the hub hands it"
        );
        assert!(
            hub.feed().is_none(),
            "a channel has one reader, so the screen cannot take it twice"
        );
    }

    #[tokio::test]
    async fn a_question_waits_for_the_key_that_answers_it() {
        let (hub, mut feed) = pair();
        let screen = tokio::spawn(async move {
            match feed.recv().await {
                Some(Msg::Approval { reply, request }) => {
                    assert_eq!(request.summary, "write notes.md");
                    reply.send(ApprovalDecision::Approved).expect("asked");
                }
                other => panic!("an approval is not an event: {other:?}"),
            }
        });
        let decision = hub.approve(&request()).await;
        assert!(decision.approved());
        screen.await.expect("the screen answered");
    }

    #[tokio::test]
    async fn a_question_no_screen_ever_shows_is_answered_no() {
        let (hub, feed) = pair();
        drop(feed);
        match hub.approve(&request()).await {
            ApprovalDecision::Rejected { reason } => {
                assert!(reason.contains("screen was gone"), "{reason}");
            }
            other => panic!("a question nobody saw is not a yes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_question_closed_without_an_answer_is_answered_no() {
        let (hub, mut feed) = pair();
        let screen = tokio::spawn(async move {
            match feed.recv().await {
                Some(Msg::Approval { reply, .. }) => drop(reply),
                other => panic!("expected a question, got {other:?}"),
            }
        });
        match hub.approve(&request()).await {
            ApprovalDecision::Rejected { reason } => {
                assert!(reason.contains("no key answered"), "{reason}");
            }
            other => panic!("silence is not consent, got {other:?}"),
        }
        screen.await.expect("the screen dropped the answer");
    }

    #[tokio::test]
    async fn an_event_with_no_screen_does_not_stop_the_run() {
        // The observer returns nothing: a run must not halt because the picture
        // went away, and it must not pretend the event never happened either — the
        // session, not this channel, is the record.
        let (hub, feed) = pair();
        drop(feed);
        hub.observe(&event());
    }
}
