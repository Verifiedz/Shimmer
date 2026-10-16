//! The loop guard (ADR 0035 §3). Automations remembers each task it queued with the chain of rules
//! that led to it; an event whose `cause` is one of those tasks continues that chain. A rule
//! already in the chain doesn't fire again (A → B → A), and a chain stops at the depth limit.
//! Pure: plain data, no `Ctx` (§12 rule 10).

use std::collections::{HashMap, VecDeque};

use shimmer_core::TaskId;

/// How many queued tasks are remembered. A task's chain only matters while it runs, and the
/// queue lives in memory too, so the oldest are dropped.
const REMEMBERED: usize = 10_000;

/// The rules that led here. `id` is the id of the event that started it.
#[derive(Clone, Debug, PartialEq)]
pub struct Chain {
    pub id: String,
    pub rules: Vec<String>,
}

/// Why a rule didn't fire.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stop {
    /// The rule is already in the chain.
    Cycle,
    /// The chain has as many rules as the depth limit allows.
    Depth,
}

impl Stop {
    pub fn as_str(self) -> &'static str {
        match self {
            Stop::Cycle => "cycle",
            Stop::Depth => "depth",
        }
    }
}

impl Chain {
    /// May `rule` fire in this chain, at most `depth` rules long? The chain its tasks continue.
    pub fn next(&self, rule: &str, depth: usize) -> Result<Chain, Stop> {
        if self.rules.iter().any(|r| r == rule) {
            return Err(Stop::Cycle);
        }
        if self.rules.len() >= depth {
            return Err(Stop::Depth);
        }
        let mut rules = self.rules.clone();
        rules.push(rule.to_owned());
        Ok(Chain { id: self.id.clone(), rules })
    }
}

#[derive(Debug, Default)]
pub struct Chains {
    by_task: HashMap<TaskId, Chain>,
    order: VecDeque<TaskId>,
}

impl Chains {
    /// The chain an event continues: its cause's, or a new one started by `event_id` (anything a
    /// person or a schedule did, §3).
    pub fn of(&self, cause: Option<TaskId>, event_id: &str) -> Chain {
        cause
            .and_then(|t| self.by_task.get(&t))
            .cloned()
            .unwrap_or_else(|| Chain { id: event_id.to_owned(), rules: Vec::new() })
    }

    pub fn remember(&mut self, task: TaskId, chain: Chain) {
        if self.by_task.insert(task, chain).is_none() {
            self.order.push_back(task);
        }
        while self.order.len() > REMEMBERED {
            if let Some(old) = self.order.pop_front() {
                self.by_task.remove(&old);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_grows_by_rule_and_stops_at_a_cycle_or_the_depth() {
        let mut chains = Chains::default();
        let start = chains.of(None, "ev1");
        assert_eq!(start, Chain { id: "ev1".into(), rules: vec![] });

        let a = start.next("a", 3).unwrap();
        let task = TaskId::new();
        chains.remember(task, a.clone());
        let continued = chains.of(Some(task), "ev2");
        assert_eq!(continued, a, "an event caused by a's task continues a's chain");

        assert_eq!(continued.next("a", 3), Err(Stop::Cycle), "A → A");
        let b = continued.next("b", 3).unwrap();
        assert_eq!(b.next("a", 3), Err(Stop::Cycle), "A → B → A");
        let c = b.next("c", 3).unwrap();
        assert_eq!(c.rules, ["a", "b", "c"]);
        assert_eq!(c.next("d", 3), Err(Stop::Depth));
        assert_eq!(start.next("a", 0), Err(Stop::Depth));

        assert_eq!(chains.of(Some(TaskId::new()), "ev3").rules.len(), 0, "a task it didn't queue starts fresh");
    }

    #[test]
    fn only_the_newest_tasks_are_remembered() {
        let mut chains = Chains::default();
        let first = TaskId::new();
        chains.remember(first, Chain { id: "x".into(), rules: vec!["a".into()] });
        for _ in 0..REMEMBERED {
            chains.remember(TaskId::new(), Chain { id: "y".into(), rules: vec![] });
        }
        assert_eq!(chains.of(Some(first), "z").id, "z", "the oldest was dropped");
        assert_eq!(chains.by_task.len(), REMEMBERED);
    }
}
