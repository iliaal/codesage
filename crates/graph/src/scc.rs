//! Iterative Tarjan strongly-connected-components over a `(from, to)` edge
//! list. Shared by the session structural diff (`session.rs`) and the
//! git-history risk cycle detection (`git_history::risk`) — both build a
//! file-level import graph and want its SCCs.

use std::collections::HashMap;

/// Iterative Tarjan's strongly-connected-components algorithm.
///
/// Standard recursive Tarjan risks a stack overflow on deep import chains
/// (php-src has some really deep include webs). The iterative form is
/// marginally more code but bounded in stack usage by the explicit work-queue
/// size.
///
/// Input: edge list `(from, to)`. Output: list of SCCs, each a `Vec<String>`
/// of node names. Nodes not in any edge are omitted (they can't be part of a
/// multi-node cycle). Order within each SCC matches finish-order from the DFS;
/// callers that need stable output should sort.
pub(crate) fn tarjan_scc(edges: &[(String, String)]) -> Vec<Vec<String>> {
    // Build adjacency and a stable node list (0-indexed).
    let mut idx_of: HashMap<&str, usize> = HashMap::new();
    let mut nodes: Vec<&str> = Vec::new();
    for (a, b) in edges {
        for n in [a.as_str(), b.as_str()] {
            if !idx_of.contains_key(n) {
                idx_of.insert(n, nodes.len());
                nodes.push(n);
            }
        }
    }
    let n = nodes.len();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (a, b) in edges {
        let u = idx_of[a.as_str()];
        let v = idx_of[b.as_str()];
        adj[u].push(v);
    }

    const UNVISITED: i32 = -1;
    let mut index_counter: i32 = 0;
    let mut index: Vec<i32> = vec![UNVISITED; n];
    let mut lowlink: Vec<i32> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut components: Vec<Vec<String>> = Vec::new();

    // Work-queue entries: (node, next-child-index-to-visit). The second element
    // encodes how far through adj[node] we've gotten so we can resume after a
    // descent without the actual call stack.
    for start in 0..n {
        if index[start] != UNVISITED {
            continue;
        }
        let mut work: Vec<(usize, usize)> = Vec::new();
        index[start] = index_counter;
        lowlink[start] = index_counter;
        index_counter += 1;
        stack.push(start);
        on_stack[start] = true;
        work.push((start, 0));

        while let Some(&(v, i)) = work.last() {
            if i < adj[v].len() {
                let w = adj[v][i];
                // Advance the parent's child cursor before descending so that
                // when we pop back we continue from the next child.
                work.last_mut().unwrap().1 = i + 1;
                if index[w] == UNVISITED {
                    index[w] = index_counter;
                    lowlink[w] = index_counter;
                    index_counter += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    work.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index[w]);
                }
            } else {
                // All children visited. If v is an SCC root, pop off the
                // component. Otherwise propagate its lowlink to the parent.
                if lowlink[v] == index[v] {
                    let mut component: Vec<String> = Vec::new();
                    loop {
                        let w = stack.pop().expect("stack underflow");
                        on_stack[w] = false;
                        component.push(nodes[w].to_string());
                        if w == v {
                            break;
                        }
                    }
                    components.push(component);
                }
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    lowlink[parent] = lowlink[parent].min(lowlink[v]);
                }
            }
        }
    }
    components
}
