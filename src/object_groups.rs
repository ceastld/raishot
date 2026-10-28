//! 组合只保存真实叶对象的层级路径；不生成组位图或虚拟几何。
use super::{Object, ObjectId};
use crate::Result;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const MAX_GROUP_DEPTH: usize = 16;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GroupMembership {
    pub(crate) group: Option<u64>,
    pub(crate) nested_groups: Vec<u64>,
}
impl GroupMembership {
    pub(crate) fn path(&self) -> Vec<u64> {
        self.group
            .into_iter()
            .chain(self.nested_groups.iter().copied())
            .collect()
    }
    pub(crate) fn from_path(path: &[u64]) -> Self {
        Self {
            group: path.first().copied(),
            nested_groups: path.get(1..).unwrap_or_default().to_vec(),
        }
    }
    pub(crate) fn validate(&self, next_group: u64) -> Result<()> {
        if self.group.is_none() && !self.nested_groups.is_empty() {
            return Err("组合内链缺少最外层组号".into());
        }
        let path = self.path();
        if path.len() > MAX_GROUP_DEPTH {
            return Err("组合层级超过16层".into());
        }
        let mut seen = BTreeSet::new();
        for id in path {
            if id == 0 || id > next_group {
                return Err("组合路径组号超出分配时钟".into());
            }
            if !seen.insert(id) {
                return Err("组合路径包含循环或重复组号".into());
            }
        }
        Ok(())
    }
}

impl Object {
    pub(crate) fn membership(&self) -> GroupMembership {
        GroupMembership {
            group: self.group,
            nested_groups: self.nested_groups.clone(),
        }
    }
    pub(crate) fn set_membership(&mut self, membership: &GroupMembership) {
        self.group = membership.group;
        self.nested_groups.clone_from(&membership.nested_groups);
    }
    pub(crate) fn group_path(&self) -> Vec<u64> {
        self.group
            .into_iter()
            .chain(self.nested_groups.iter().copied())
            .collect()
    }
    pub(crate) fn in_group_scope(&self, scope: &[u64]) -> bool {
        scope.len() <= usize::from(self.group.is_some()) + self.nested_groups.len()
            && scope.iter().copied().eq(self
                .group
                .into_iter()
                .chain(self.nested_groups.iter().copied())
                .take(scope.len()))
    }
}

/// 只验证同一个实际文档状态，不能合并不同时代的历史 before/after。
pub(crate) fn validate_forest(objects: &[Object], next_group: u64) -> Result<usize> {
    let mut prefixes = BTreeMap::<u64, Vec<u64>>::new();
    let mut inner_references = 0_usize;
    for object in objects {
        let membership = object.membership();
        membership.validate(next_group)?;
        inner_references = inner_references
            .checked_add(membership.nested_groups.len())
            .ok_or("组合内链引用数量溢出")?;
        let path = membership.path();
        for (index, &id) in path.iter().enumerate() {
            let prefix = &path[..index];
            if let Some(previous) = prefixes.get(&id) {
                if previous != prefix {
                    return Err("同一组合在当前文档具有多个祖先或父组".into());
                }
            } else {
                prefixes.insert(id, prefix.to_vec());
            }
        }
    }
    Ok(inner_references)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum GroupUnit {
    Group(u64),
    Object(ObjectId),
}

pub(crate) fn scope_contains(object: &Object, scope: &[u64]) -> bool {
    object.in_group_scope(scope)
}
pub(crate) fn unit(object: &Object, scope: &[u64]) -> Option<GroupUnit> {
    if !scope_contains(object, scope) {
        return None;
    }
    let group = if scope.is_empty() {
        object.group
    } else {
        object.nested_groups.get(scope.len() - 1).copied()
    };
    Some(group.map_or(GroupUnit::Object(object.id), GroupUnit::Group))
}
pub(crate) fn valid_scope(objects: &[Object], scope: &[u64]) -> Vec<u64> {
    if scope.is_empty() {
        return Vec::new();
    }
    for depth in (1..=scope.len().min(MAX_GROUP_DEPTH)).rev() {
        let candidate = &scope[..depth];
        if objects
            .iter()
            .any(|object| scope_contains(object, candidate))
        {
            return candidate.to_vec();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        document::{BoundedHistorySnapshot, Change, Document},
        geometry::Rect,
    };

    fn rectangle(x: f32) -> Annotation {
        let mut a = Annotation::new(Tool::Rectangle, Point { x, y: 20. }, Style::default());
        a.update(Point { x: x + 30., y: 50. });
        a
    }
    fn nested() -> (Document, [ObjectId; 3], u64, u64) {
        let mut d = Document::default();
        let ids = [
            d.insert(rectangle(20.)),
            d.insert(rectangle(80.)),
            d.insert(rectangle(150.)),
        ];
        assert!(d.set_group(&ids[..2], true));
        let child = d.items()[0].group.unwrap();
        assert!(d.set_group(&[ids[0], ids[2]], true));
        let root = d.items()[0].group.unwrap();
        (d, ids, root, child)
    }
    fn paths(d: &Document) -> Vec<Vec<u64>> {
        d.items().iter().map(Object::group_path).collect()
    }
    #[test]
    fn nested_wrap_pop_keeps_children_and_true_two_stacks_without_geometry_edits() {
        let (mut d, ids, root, child) = nested();
        let annotations = d
            .items()
            .iter()
            .map(|o| o.annotation.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            paths(&d),
            vec![vec![root, child], vec![root, child], vec![root]]
        );
        assert_eq!(d.expand_groups(&[ids[0]]), ids);
        let before = d.bounded_history_snapshot().unwrap();
        assert!(matches!(
            before.undo.last(),
            Some(Change::NestedGroups { .. })
        ));
        let revisions = (d.revision(), d.edit_revision());
        assert!(d.set_group(&[ids[0]], false));
        assert_eq!(d.revision(), revisions.0 + 1);
        assert_eq!(d.edit_revision(), revisions.1 + 1);
        assert_eq!(paths(&d), vec![vec![child], vec![child], vec![]]);
        assert_eq!(
            d.items()
                .iter()
                .map(|o| o.annotation.clone())
                .collect::<Vec<_>>(),
            annotations
        );
        d.undo();
        assert_eq!(d.items(), before.items);
        let saved = d.bounded_history_snapshot().unwrap();
        assert!(!saved.undo.is_empty() && !saved.redo.is_empty());
        let mut restored = Document::from_bounded_history_snapshot(saved.clone()).unwrap();
        assert_eq!(restored.bounded_history_snapshot().unwrap(), saved);
        restored.redo();
        assert_eq!(paths(&restored), vec![vec![child], vec![child], vec![]]);
        restored.undo();
        assert_eq!(restored.items(), before.items);
        restored.undo();
        assert_eq!(paths(&restored), vec![vec![child], vec![child], vec![]]);
        restored.redo();
        assert_eq!(restored.items(), before.items);
    }
    #[test]
    fn nested_scope_groups_only_current_units_and_ungroups_one_level_with_legacy_tag() {
        let (mut d, ids, root, child) = nested();
        let original = d.items().to_vec();
        assert_eq!(d.expand_groups_in_scope(&[ids[0]], &[root]), ids[..2]);
        assert_eq!(
            d.expand_groups_in_scope(&[ids[0]], &[root, child]),
            vec![ids[0]]
        );
        assert!(!d.can_group_in_scope(&ids[..2], &[root]));
        assert!(d.can_group_in_scope(&[ids[0], ids[2]], &[root]));
        assert!(d.set_group_in_scope(&[ids[0], ids[2]], &[root], true));
        let middle = d.items()[0].nested_groups[0];
        assert_eq!(
            paths(&d),
            vec![
                vec![root, middle, child],
                vec![root, middle, child],
                vec![root, middle]
            ]
        );
        assert!(d.set_group_in_scope(&[ids[0]], &[root], false));
        assert_eq!(d.items(), original);
        d.undo();
        assert_eq!(d.items()[0].group_path(), vec![root, middle, child]);
        d.redo();
        assert_eq!(d.items(), original);
        let mut flat = Document::default();
        let a = flat.insert(rectangle(0.));
        let b = flat.insert(rectangle(40.));
        assert!(flat.set_group(&[a, b], true));
        assert!(matches!(
            flat.bounded_history_snapshot().unwrap().undo.last(),
            Some(Change::Groups { .. })
        ));
        assert!(flat.set_group(&[a], false));
        assert!(matches!(
            flat.bounded_history_snapshot().unwrap().undo.last(),
            Some(Change::Groups { .. })
        ));
    }
    #[test]
    fn nested_duplicate_remaps_descendants_keeps_scope_and_remove_restores_complete_membership() {
        let (mut d, ids, root, child) = nested();
        let before = d.items().to_vec();
        let revision = d.edit_revision();
        let commands = d.bounded_history_snapshot().unwrap().undo.len();
        let copies = d.duplicate_in_group_scope(&ids[..2], Rect::full(400, 300), &[root]);
        assert_eq!(copies.len(), 2);
        assert_eq!(d.edit_revision(), revision + 1);
        assert_eq!(
            d.bounded_history_snapshot().unwrap().undo.len(),
            commands + 1
        );
        let copied = &d.items()[3..];
        assert_eq!(copied[0].group, Some(root));
        assert_eq!(copied[0].nested_groups, copied[1].nested_groups);
        assert_ne!(copied[0].nested_groups, vec![child]);
        assert_eq!(
            copied[0].annotation.points[0].x,
            before[0].annotation.points[0].x + 12.
        );
        d.undo();
        assert_eq!(d.items(), before);
        d.redo();
        assert_eq!(d.items()[3].id, copies[0]);
        d.remove_many(&ids[..2]);
        assert_eq!(d.valid_group_scope(&[root, child]), vec![root]);
        d.undo();
        assert_eq!(d.items()[0].group_path(), vec![root, child]);
        assert_eq!(d.items()[1].group_path(), vec![root, child]);
        assert!(d.bounded_history_snapshot().is_ok());
    }
    #[test]
    fn nested_invalid_forests_old_group_payload_and_missing_history_members_reject() {
        let (d, ids, root, child) = nested();
        let valid = d.bounded_history_snapshot().unwrap();
        let mut bad_states = Vec::new();
        let mut missing_root = valid.clone();
        missing_root.items[0].group = None;
        bad_states.push(missing_root);
        let mut cycle = valid.clone();
        cycle.items[0].nested_groups = vec![root];
        bad_states.push(cycle);
        let mut parents = valid.clone();
        parents.items[2].group = Some(child);
        bad_states.push(parents);
        let mut clock = valid.clone();
        clock.items[0].nested_groups = vec![valid.next_group + 1];
        bad_states.push(clock);
        let mut depth = valid.clone();
        depth.next_group = 17;
        depth.items[0].group = Some(1);
        depth.items[0].nested_groups = (2..=17).collect();
        bad_states.push(depth);
        for bad in bad_states {
            assert!(Document::from_bounded_history_snapshot(bad).is_err());
        }
        let mut legacy_over_nested = valid.clone();
        legacy_over_nested.undo = vec![Change::Groups {
            before: vec![(ids[0], None)],
            after: vec![(ids[0], Some(root))],
        }];
        legacy_over_nested.redo.clear();
        assert!(Document::from_bounded_history_snapshot(legacy_over_nested).is_err());
        let mut missing_member = valid;
        missing_member.undo = vec![Change::NestedGroups {
            before: vec![(999, GroupMembership::default())],
            after: vec![(999, GroupMembership::from_path(&[root]))],
        }];
        missing_member.redo.clear();
        assert!(Document::from_bounded_history_snapshot(missing_member).is_err());
    }
    #[test]
    fn nested_depth_clock_and_same_unit_noop_preserve_redo_and_allocators() {
        let deep = (1..=16).collect::<Vec<_>>();
        let leaf = &deep[..15];
        let object = |id, path: &[u64]| {
            let membership = GroupMembership::from_path(path);
            Object {
                id,
                annotation: rectangle(id as f32 * 40.),
                group: membership.group,
                nested_groups: membership.nested_groups,
            }
        };
        let mut d = Document::from_bounded_history_snapshot(BoundedHistorySnapshot {
            items: vec![object(1, &deep), object(2, &deep), object(3, leaf)],
            crop_mask: None,
            long_clip: None,
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 3,
            next_group: 16,
        })
        .unwrap();
        let extra = d.insert(rectangle(200.));
        d.undo();
        let before = d.bounded_history_snapshot().unwrap();
        let revision = d.revision();
        assert!(!d.set_group_in_scope(&[1, 2], leaf, true));
        assert!(!d.set_group_in_scope(&[1, 3], leaf, true));
        assert!(!d.can_group_in_scope(&[1, 3], leaf));
        assert!(!d.set_group_in_scope(&[1, 3], &[999], true));
        assert_eq!(d.revision(), revision);
        assert_eq!(d.bounded_history_snapshot().unwrap(), before);
        assert!(d.can_redo());
        d.redo();
        assert_eq!(d.items().last().unwrap().id, extra);
        let (mut exhausted, ids, _, _) = nested();
        exhausted.next_group = u32::MAX as u64;
        let state = exhausted.bounded_history_snapshot().unwrap();
        assert!(exhausted.duplicate(&ids, Rect::full(400, 300)).is_empty());
        assert_eq!(exhausted.bounded_history_snapshot().unwrap(), state);
    }
}
