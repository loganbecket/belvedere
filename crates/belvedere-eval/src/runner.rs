//! Runners turn a case into an outcome. `AgentRunner` (in its own file)
//! asks a real model; `Scripted` answers from a table, for testing the
//! harness itself.

#[cfg(test)]
use std::collections::HashMap;

use crate::cases::Case;
use crate::score::Outcome;

/// Something that can run a case.
pub trait Runner {
    fn run(&mut self, case: &Case) -> impl std::future::Future<Output = Outcome> + Send;
    /// A short description for the results file ("Qwen3.5 9B on GPU").
    fn describe(&self) -> String;
}

/// Fixed answers keyed by case id. Unknown ids get the unchanged outcome.
#[cfg(test)]
#[derive(Default)]
pub struct Scripted {
    pub outcomes: HashMap<String, Outcome>,
}

#[cfg(test)]
impl Runner for Scripted {
    async fn run(&mut self, case: &Case) -> Outcome {
        self.outcomes
            .get(&case.id)
            .cloned()
            .unwrap_or_else(|| Outcome::unchanged(&case.tasks))
    }

    fn describe(&self) -> String {
        "scripted".into()
    }
}

#[cfg(test)]
mod tests {}
