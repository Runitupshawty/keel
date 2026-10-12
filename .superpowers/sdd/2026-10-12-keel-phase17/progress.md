# SDD ledger - plan: docs/superpowers/plans/2026-10-12-keel-phase17.md
Task 82: started from e4dadcf (origin/p17-scan).
Scope: requested window, picker, tree, all columns, extension list and shared selection; treemap integration slot only.
Design: Arc<Tree> scan snapshot; visible rows cached until expand/filter/sort changes; extension aggregation built once; OS descriptions and optional metadata on workers. Per-scan receiver prevents stale workers replacing newer results. Use scan defaults for cloud/link safety.
Ruling: follow the explicit request's treemap placeholder and focus on its listed Task 82 features; Explorer registration and additional pane view remain follow-up integration work.
Tests: initial model contract committed 9bed10e; remote RED queued.
