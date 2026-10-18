use crate::model::*;

/// Pure queue planning: which queued tasks start next given caps. Sorted by order, then priority High>Normal>Low, then FIFO.
pub fn plan_starts(
    tasks: &[DownloadTask],
    queues: &[Queue],
    active: &[(String, String)],
    global_max: usize,
) -> Vec<String> {
    let mut global_active = active.len();
    let mut per_queue: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (_, qid) in active {
        *per_queue.entry(qid.clone()).or_default() += 1;
    }
    let mut candidates: Vec<&DownloadTask> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Queued)
        .collect();
    candidates.sort_by(|a, b| {
        a.order
            .cmp(&b.order)
            .then(priority_rank(b.priority).cmp(&priority_rank(a.priority)))
    });
    let mut out = Vec::new();
    for t in candidates {
        if global_active >= global_max && global_max > 0 {
            break;
        }
        let qmax = queues
            .iter()
            .find(|q| q.id == t.queue_id)
            .map(|q| q.max_concurrent)
            .unwrap_or(1);
        let cur = *per_queue.get(&t.queue_id).unwrap_or(&0);
        if cur >= qmax {
            continue;
        }
        *per_queue.entry(t.queue_id.clone()).or_default() += 1;
        global_active += 1;
        out.push(t.id.clone());
    }
    out
}
fn priority_rank(p: Priority) -> u8 {
    match p {
        Priority::High => 2,
        Priority::Normal => 1,
        Priority::Low => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t(id: &str, q: &str, order: i64, pr: Priority) -> DownloadTask {
        DownloadTask {
            id: id.into(),
            url: "https://x/f".into(),
            filename: "f".into(),
            queue_id: q.into(),
            order,
            priority: pr,
            status: TaskStatus::Queued,
            ..Default::default()
        }
    }
    #[test]
    fn respects_caps_and_priority() {
        let tasks = vec![
            t("a", "main", 2, Priority::Low),
            t("b", "main", 1, Priority::High),
            t("c", "main", 3, Priority::High),
            t("d", "q2", 4, Priority::Normal),
        ];
        let queues = vec![
            Queue::default(),
            Queue {
                id: "q2".into(),
                name: "Q2".into(),
                max_concurrent: 1,
                ..Default::default()
            },
        ];
        // order dominates, then priority among same order is n/a here; main cap 3, global 2
        let plan = plan_starts(&tasks, &queues, &[], 2);
        assert_eq!(plan, vec!["b", "a"]);
    }
    #[test]
    fn per_queue_cap() {
        let tasks = vec![
            t("a", "main", 1, Priority::Normal),
            t("b", "main", 2, Priority::Normal),
            t("c", "q2", 3, Priority::Normal),
        ];
        let q = Queue {
            max_concurrent: 1,
            ..Default::default()
        };
        let queues = vec![
            q,
            Queue {
                id: "q2".into(),
                name: "Q2".into(),
                max_concurrent: 1,
                ..Default::default()
            },
        ];
        let plan = plan_starts(&tasks, &queues, &[], 10);
        assert_eq!(plan, vec!["a", "c"]);
    }
}
