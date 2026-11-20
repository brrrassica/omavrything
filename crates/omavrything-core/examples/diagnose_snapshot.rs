//! Diagnostic: load the live snapshot and simulate `SubtreeIndex::collect_subtree`
//! (the traversal whose `out: Vec<u32>` doubles unboundedly in production).
//!
//! Builds the exact same first_child/next_sibling links as `SubtreeIndex::build`,
//! checks the sibling chains for cycles / duplicate visits, then runs the
//! `collect_subtree` walk from every parentless record with a step budget.

use omavrything_core::snapshot;

const MAX: u32 = u32::MAX;
const BUDGET: u64 = 200_000_000;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "om.snapshot".to_string());
    let bytes = std::fs::read(&path).expect("read snapshot");
    let sections = snapshot::sections_from_bytes(&bytes).expect("sections");
    let image = sections.image(&bytes).expect("image");
    let records: &[omavrything_core::index::FileRecord] = image.records();
    let n = records.len();
    eprintln!("records: {n}");

    // --- rebuild links exactly as SubtreeIndex::build_inner ---
    let mut first_child = vec![MAX; n];
    let mut next_sibling = vec![MAX; n];
    let mut parentless: Vec<u32> = Vec::new();
    for (i, rec) in records.iter().enumerate() {
        let idx = i as u32;
        let parent = rec.parent;
        if parent == MAX {
            parentless.push(idx);
        } else if (parent as usize) < n {
            let p = parent as usize;
            next_sibling[i] = first_child[p];
            first_child[p] = idx;
        }
    }
    eprintln!("parentless: {}", parentless.len());

    // --- check sibling chains for cycles / duplicate idx visits ---
    let mut chain_bad = 0usize;
    for (head_idx, &head) in first_child.iter().enumerate() {
        let mut child = head;
        if child == MAX {
            continue;
        }
        let mut steps = 0u64;
        let mut seen: Vec<u32> = Vec::new();
        while child != MAX {
            if steps > BUDGET {
                eprintln!("sibling chain from {head_idx} exceeds budget");
                chain_bad += 1;
                break;
            }
            seen.push(child);
            steps += 1;
            if steps > n as u64 + 1 {
                eprintln!(
                    "SIBLING CYCLE under record {head_idx}: chain revisits {child} at step {steps}; \
                     chain tail (last 12): {:?}",
                    &seen[seen.len().saturating_sub(12)..]
                );
                chain_bad += 1;
                break;
            }
            child = next_sibling[child as usize];
        }
        if chain_bad >= 3 {
            break;
        }
    }
    if chain_bad == 0 {
        eprintln!("no sibling-chain cycles");
    }

    // --- simulate collect_subtree from every parentless record ---
    // Exact semantics from index.rs:473-497.
    'roots: for &root in &parentless {
        if records[root as usize].is_deleted() {
            continue;
        }
        let mut out: Vec<u32> = Vec::new();
        let mut cursor = 0usize;
        out.push(root);
        let mut steps = 0u64;
        while cursor < out.len() {
            let dir = out[cursor];
            cursor += 1;
            let mut child = first_child.get(dir as usize).copied().unwrap_or(MAX);
            while child != MAX {
                if records[child as usize].parent == dir {
                    out.push(child);
                }
                child = next_sibling.get(child as usize).copied().unwrap_or(MAX);
                steps += 1;
                if steps > BUDGET {
                    eprintln!(
                        "collect_subtree from parentless {root}: RUNAWAY after {steps} steps, \
                         out.len()={} (dir={dir}, child={child})",
                        out.len()
                    );
                    // print the chain around the runaway
                    let mut tail = Vec::new();
                    let mut c = child;
                    for _ in 0..16 {
                        tail.push(c);
                        if c == MAX {
                            break;
                        }
                        c = next_sibling[c as usize];
                    }
                    eprintln!("  child chain from {child}: {tail:?}");
                    for &i in tail.iter().take(6) {
                        if i == MAX {
                            continue;
                        }
                        let rec = &records[i as usize];
                        eprintln!(
                            "    {i}: parent={} dir={} deleted={} depth={}",
                            rec.parent,
                            rec.is_dir(),
                            rec.is_deleted(),
                            rec.depth
                        );
                    }
                    break 'roots;
                }
                if out.len() > 4_000_000 {
                    eprintln!(
                        "collect_subtree from parentless {root}: out grew past 4M entries \
                         (index only has {n} records) at dir={dir}"
                    );
                    break 'roots;
                }
            }
        }
        let _ = steps;
    }
    eprintln!("done");
}
