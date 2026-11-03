//! Dependency order of database objects (M8, `docs/m8-contracts.md` §3.3): which tables must
//! exist (and be filled) before which, from foreign keys; which views, routines and triggers
//! need which objects. Structure sync, transfer and restore all create objects in
//! [`DependencyGraph::order`] and drop them in its reverse.
//!
//! The graph is engine-neutral: nodes are [`ObjectRef`]s, an edge says "this node needs that
//! one first". Where the edges come from is the caller's business: [`table_dependencies`] reads
//! foreign keys; the structure-sync stream adds view, routine and trigger edges.
//!
//! # Order
//!
//! [`DependencyGraph::order`] is deterministic: nodes are visited in insertion order and each
//! node's dependencies are emitted before it (a depth-first walk; Tarjan's strongly connected
//! components, iterative so a long chain cannot overflow a stack). Nodes on a dependency cycle
//! (two tables whose foreign keys reference each other) are emitted together, in insertion
//! order, and reported in [`DependencyOrder::cycles`], so the caller can break the cycle (create
//! the tables first, add the foreign keys after every table exists). A self-reference (a
//! foreign key to its own table) is not a dependency.

use std::collections::HashMap;

use oxbase_types::{ObjectRef, TableDef};

/// Objects and what each needs first. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    nodes: Vec<ObjectRef>,
    index: HashMap<ObjectRef, usize>,
    /// `needs[i]`: indexes node `i` depends on, sorted and unique.
    needs: Vec<Vec<usize>>,
}

/// The result of [`DependencyGraph::order`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DependencyOrder {
    /// Every node once, dependencies first.
    pub order: Vec<ObjectRef>,
    /// Groups of nodes that depend on each other (each in insertion order), in the order they
    /// appear in `order`.
    pub cycles: Vec<Vec<ObjectRef>>,
}

impl DependencyGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `node` (once; a second add is a no-op) and returns its index.
    pub fn add(&mut self, node: ObjectRef) -> usize {
        if let Some(&i) = self.index.get(&node) {
            return i;
        }
        let i = self.nodes.len();
        self.index.insert(node.clone(), i);
        self.nodes.push(node);
        self.needs.push(Vec::new());
        i
    }

    /// Records that `node` needs `dependency` first (adding both). A self-edge is ignored.
    pub fn depends_on(&mut self, node: &ObjectRef, dependency: &ObjectRef) {
        let n = self.add(node.clone());
        let d = self.add(dependency.clone());
        if n == d {
            return;
        }
        let needs = &mut self.needs[n];
        if let Err(pos) = needs.binary_search(&d) {
            needs.insert(pos, d);
        }
    }

    /// Number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// `true` when `node` was added.
    pub fn contains(&self, node: &ObjectRef) -> bool {
        self.index.contains_key(node)
    }

    /// What `node` needs first (direct dependencies, in insertion order of the dependencies).
    pub fn dependencies(&self, node: &ObjectRef) -> Vec<&ObjectRef> {
        self.index.get(node).map_or_else(Vec::new, |&i| {
            self.needs[i].iter().map(|&d| &self.nodes[d]).collect()
        })
    }

    /// Every node, dependencies first (module docs, *Order*).
    pub fn order(&self) -> DependencyOrder {
        let count = self.nodes.len();
        let mut number: Vec<Option<usize>> = vec![None; count];
        let mut low = vec![0_usize; count];
        let mut on_stack = vec![false; count];
        let mut stack: Vec<usize> = Vec::new();
        let mut next = 0_usize;
        let mut out = DependencyOrder::default();

        for start in 0..count {
            if number[start].is_some() {
                continue;
            }
            // (node, position in its dependency list)
            let mut calls: Vec<(usize, usize)> = vec![(start, 0)];
            number[start] = Some(next);
            low[start] = next;
            next += 1;
            stack.push(start);
            on_stack[start] = true;

            while let Some(top) = calls.len().checked_sub(1) {
                let (v, pos) = calls[top];
                if let Some(&w) = self.needs[v].get(pos) {
                    calls[top].1 += 1;
                    match number[w] {
                        None => {
                            number[w] = Some(next);
                            low[w] = next;
                            next += 1;
                            stack.push(w);
                            on_stack[w] = true;
                            calls.push((w, 0));
                        }
                        Some(nw) if on_stack[w] => low[v] = low[v].min(nw),
                        Some(_) => {}
                    }
                    continue;
                }
                calls.pop();
                if let Some(&(u, _)) = calls.last() {
                    low[u] = low[u].min(low[v]);
                }
                if Some(low[v]) == number[v] {
                    let mut component = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        component.push(w);
                        if w == v {
                            break;
                        }
                    }
                    component.sort_unstable();
                    let refs: Vec<ObjectRef> =
                        component.iter().map(|&i| self.nodes[i].clone()).collect();
                    if refs.len() > 1 {
                        out.cycles.push(refs.clone());
                    }
                    out.order.extend(refs);
                }
            }
        }
        out
    }
}

/// `true` when a foreign key's `target` names `table`: same name, and the same schema and
/// catalog where both sides set them.
pub fn same_table(target: &ObjectRef, table: &ObjectRef) -> bool {
    fn part(a: Option<&str>, b: Option<&str>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
    target.name == table.name
        && part(target.schema.as_deref(), table.schema.as_deref())
        && part(target.catalog.as_deref(), table.catalog.as_deref())
}

/// The foreign-key graph of `tables`: every table is a node (in the given order), and a table
/// depends on each other table of the set its foreign keys reference ([`same_table`]). Foreign
/// keys to tables outside the set add nothing.
pub fn table_dependencies(tables: &[TableDef]) -> DependencyGraph {
    let mut graph = DependencyGraph::new();
    for t in tables {
        graph.add(t.obj.clone());
    }
    for t in tables {
        for fk in &t.foreign_keys {
            if let Some(parent) = tables.iter().find(|u| same_table(&fk.ref_table, &u.obj)) {
                graph.depends_on(&t.obj, &parent.obj);
            }
        }
    }
    graph
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{Extras, FkAction, ForeignKeyDef, ObjectKind};
    use pretty_assertions::assert_eq;

    fn t(name: &str) -> ObjectRef {
        ObjectRef::table(None, Some("s"), name)
    }

    fn names(refs: &[ObjectRef]) -> Vec<&str> {
        refs.iter().map(|r| r.name.as_str()).collect()
    }

    fn table(name: &str, refs: &[&str]) -> TableDef {
        let mut def = TableDef::new(t(name));
        for (i, to) in refs.iter().enumerate() {
            def.foreign_keys.push(ForeignKeyDef {
                name: Some(format!("{name}_fk{i}")),
                columns: vec!["p".into()],
                // Foreign keys often omit the catalog; they still match.
                ref_table: ObjectRef::table(None, Some("s"), *to),
                ref_columns: vec!["id".into()],
                on_delete: FkAction::NoAction,
                on_update: FkAction::NoAction,
                extras: Extras::default(),
            });
        }
        def
    }

    #[test]
    fn dependencies_come_first_and_ties_keep_insertion_order() {
        let tables = [
            table("order_items", &["orders", "products"]),
            table("orders", &["customers"]),
            table("customers", &[]),
            table("products", &[]),
            table("notes", &["outside"]),
            table("tree", &["tree"]),
        ];
        let graph = table_dependencies(&tables);
        assert_eq!(graph.len(), 6);
        assert_eq!(
            names(&graph.order().order),
            [
                "customers",
                "orders",
                "products",
                "order_items",
                "notes",
                "tree"
            ]
        );
        assert_eq!(graph.order().cycles, Vec::<Vec<ObjectRef>>::new());
        assert_eq!(
            graph
                .dependencies(&t("order_items"))
                .into_iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["orders", "products"]
        );
        // A self-reference is no dependency.
        assert_eq!(graph.dependencies(&t("tree")), Vec::<&ObjectRef>::new());
    }

    #[test]
    fn cycles_are_emitted_together_and_reported() {
        let tables = [
            table("a", &["b"]),
            table("b", &["a"]),
            table("c", &["a"]),
            table("d", &[]),
        ];
        let order = table_dependencies(&tables).order();
        assert_eq!(names(&order.order), ["a", "b", "c", "d"]);
        assert_eq!(order.cycles.len(), 1);
        assert_eq!(names(&order.cycles[0]), ["a", "b"]);
    }

    #[test]
    fn mixed_kinds_and_long_chains() {
        let mut graph = DependencyGraph::new();
        let view = ObjectRef::new(ObjectKind::View, "v").in_schema(Some("s"));
        graph.depends_on(&view, &t("x"));
        graph.add(t("x"));
        assert_eq!(graph.len(), 2);
        assert!(graph.contains(&view));
        assert_eq!(names(&graph.order().order), ["x", "v"]);

        // 50 000 tables in one chain: no recursion, so no stack overflow.
        let mut chain = DependencyGraph::new();
        for i in 0..50_000 {
            chain.depends_on(&t(&format!("n{i}")), &t(&format!("n{}", i + 1)));
        }
        let order = chain.order();
        assert_eq!(order.order.len(), 50_001);
        assert_eq!(order.order[0].name, "n50000");
        assert_eq!(order.order[50_000].name, "n0");
        assert_eq!(
            DependencyGraph::new().order().order,
            Vec::<ObjectRef>::new()
        );
    }

    #[test]
    fn foreign_key_targets_match_loosely() {
        let full = ObjectRef::table(Some("db"), Some("s"), "t");
        assert!(same_table(&ObjectRef::table(None, Some("s"), "t"), &full));
        assert!(same_table(&ObjectRef::table(None, None, "t"), &full));
        assert!(!same_table(&ObjectRef::table(None, Some("o"), "t"), &full));
        assert!(!same_table(
            &ObjectRef::table(Some("x"), Some("s"), "t"),
            &full
        ));
        assert!(!same_table(&ObjectRef::table(None, Some("s"), "u"), &full));
    }
}
