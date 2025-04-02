use std::collections::vec_deque::{IntoIter, Iter, IterMut};
use std::collections::VecDeque;
use std::ops::{Index, IndexMut};
use std::rc::Rc;

//use smithay::reexports::rustix::event::epoll::Iter;

use super::monitor::WorkspaceSwitch;
use super::workspace::{Workspace, WorkspaceId};
use super::{LayoutElement, Options};
use crate::animation::{Animation, Clock};

use smithay::output::Output;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct WorkspaceIdx(isize);

impl WorkspaceIdx {
    fn decrement(&mut self) {
        self.0 -= 1;
    }
}

#[derive(Debug)]
pub struct Workspaces<W: LayoutElement> {
    /// associated output
    pub(super) output: Output,
    /// Clock for driving animations.
    pub(super) clock: Clock,
    /// Configurable properties of the layout.
    pub(super) options: Rc<Options>,

    workspaces: VecDeque<Workspace<W>>,
    active_workspace_idx: WorkspaceIdx,
    workspace_idx_offset: usize,
    previous_workspace_id: Option<WorkspaceId>,
    workspace_switch: Option<WorkspaceSwitch>,
}

impl<W: LayoutElement> Index<WorkspaceIdx> for Workspaces<W> {
    type Output = Workspace<W>;

    fn index(&self, index: WorkspaceIdx) -> &Self::Output {
        let internal_index = self.to_internal_index(index);
        &self.workspaces[internal_index]
    }
}

impl<W: LayoutElement> IndexMut<WorkspaceIdx> for Workspaces<W> {
    fn index_mut(&mut self, index: WorkspaceIdx) -> &mut Self::Output {
        let internal_index = self.to_internal_index(index);
        &mut self.workspaces[internal_index]
    }
}

impl<W: LayoutElement> Index<WorkspaceId> for Workspaces<W> {
    type Output = Workspace<W>;

    fn index(&self, id: WorkspaceId) -> &Self::Output {
        let internal_index = self
            .workspaces
            .iter()
            .position(|w| w.id() == id)
            .expect("no workspace with this id");
        &self.workspaces[internal_index]
    }
}

impl<W: LayoutElement> IndexMut<WorkspaceId> for Workspaces<W> {
    fn index_mut(&mut self, id: WorkspaceId) -> &mut Self::Output {
        let internal_index = self
            .workspaces
            .iter()
            .position(|w| w.id() == id)
            .expect("no workspace with this id");
        &mut self.workspaces[internal_index]
    }
}

impl<W: LayoutElement> Workspaces<W> {
    pub fn new(
        workspaces: Vec<Workspace<W>>,
        output: Output,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        Self {
            output,
            clock,
            options,
            workspaces: workspaces.into(),
            active_workspace_idx: WorkspaceIdx(0),
            workspace_idx_offset: 0,
            previous_workspace_id: None,
            workspace_switch: None,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.workspaces.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    #[inline]
    fn to_internal_index(&self, external_index: WorkspaceIdx) -> usize {
        debug_assert!(external_index.0 + self.workspace_idx_offset as isize > 0);
        (external_index.0 + self.workspace_idx_offset as isize) as usize
    }

    #[inline]
    pub fn to_external_index(&self, internal_index: usize) -> WorkspaceIdx {
        WorkspaceIdx(internal_index as isize - self.workspace_idx_offset as isize)
    }

    #[inline]
    fn internal_active_workspace_idx(&self) -> usize {
        self.to_internal_index(self.active_workspace_idx)
    }

    #[inline]
    pub fn index_of_id(&self, id: WorkspaceId) -> Option<WorkspaceIdx> {
        self.workspaces
            .iter()
            .position(|w| w.id() == id)
            .map(|p| self.to_external_index(p))
    }

    /// add an element to the front without modifying the active_workspace_idx
    #[inline]
    pub fn push_front(&mut self, workspace: Workspace<W>) {
        self.workspaces.push_front(workspace);
        self.workspace_idx_offset += 1;
    }

    /// remove an element from the front without modifying the active_workspace_idx
    #[inline]
    pub fn pop_front(&mut self) -> Workspace<W> {
        if self.workspaces.len() > 1 {
            self.workspace_idx_offset += 1;
            self.workspaces
                .pop_front()
                .expect("should always contain element to pop")
        } else {
            panic!("no workspace to pop")
        }
    }

    #[inline]
    pub fn push_back(&mut self, workspace: Workspace<W>) {
        self.workspaces.push_back(workspace);
    }

    #[inline]
    pub fn pop_back(&mut self) -> Workspace<W> {
        if self.workspaces.len() > 1 {
            self.workspace_idx_offset += 1;
            self.workspaces
                .pop_back()
                .expect("should always contain element to pop")
        } else {
            panic!("no workspace to pop")
        }
    }

    #[inline]
    pub fn get(&self, index: WorkspaceIdx) -> Option<&Workspace<W>> {
        self.workspaces.get(self.to_internal_index(index))
    }

    #[inline]
    pub fn get_mut(&mut self, index: WorkspaceIdx) -> Option<&mut Workspace<W>> {
        self.workspaces.get_mut(self.to_internal_index(index))
    }

    pub fn clean_up_workspaces(&mut self) {
        assert!(self.workspace_switch.is_none());

        let range_start = if self.options.empty_workspace_above_first {
            1
        } else {
            0
        };
        for internal_idx in (range_start..self.workspaces.len() - 1).rev() {
            if self.internal_active_workspace_idx() == internal_idx {
                continue;
            }

            if !self.workspaces[internal_idx].has_windows_or_name() {
                self.workspaces.remove(internal_idx);
                if self.internal_active_workspace_idx() > internal_idx {
                    self.active_workspace_idx.decrement();
                }
            }
        }

        // Special case handling when empty_workspace_above_first is set and all workspaces
        // are empty.
        if self.options.empty_workspace_above_first && self.workspaces.len() == 2 {
            assert!(!self.workspaces[0].has_windows_or_name());
            assert!(!self.workspaces[1].has_windows_or_name());
            self.workspaces.remove(1);
            self.active_workspace_idx = WorkspaceIdx(0);
            assert!(self.workspace_idx_offset == 0);
        }

        // add back workspaces if any are missing, e.g if a new column was added
        if workspace_idx == self.workspaces.len() - 1 {
            self.add_workspace_bottom();
        }
        if self.options.empty_workspace_above_first && workspace_idx == 0 {
            self.add_workspace_top();
            workspace_idx += 1;
        }
    }

    #[inline]
    pub fn active_workspace_idx(&self) -> WorkspaceIdx {
        self.active_workspace_idx
    }

    #[inline]
    pub fn active_workspace_id(&self) -> WorkspaceId {
        self.active_workspace_ref().id()
    }

    #[inline]
    pub fn top_workspace_idx(&self) -> WorkspaceIdx {
        WorkspaceIdx(-(self.workspace_idx_offset as isize))
    }

    #[inline]
    pub fn bottom_workspace_idx(&self) -> WorkspaceIdx {
        WorkspaceIdx(self.workspaces.len() as isize - self.workspace_idx_offset as isize)
    }

    #[inline]
    pub fn active_workspace_ref(&self) -> &Workspace<W> {
        &self[self.active_workspace_idx]
    }

    #[inline]
    pub fn active_workspace(&mut self) -> &mut Workspace<W> {
        let idx = self.active_workspace_idx;
        &mut self[idx]
    }

    pub fn activate_workspace(&mut self, idx: WorkspaceIdx) {
        if self.active_workspace_idx == idx {
            return;
        }

        // FIXME: also compute and use current velocity.
        let current_idx = self
            .workspace_switch
            .as_ref()
            .map(|s| s.current_idx())
            .unwrap_or(self.internal_active_workspace_idx() as f64);

        self.previous_workspace_id = Some(self.active_workspace().id());

        self.active_workspace_idx = idx;

        self.workspace_switch = Some(WorkspaceSwitch::Animation(Animation::new(
            self.clock.clone(),
            current_idx,
            self.to_internal_index(idx) as f64,
            0.,
            self.options.animations.workspace_switch.0,
        )));
    }

    pub fn previous_workspace_idx(&self) -> Option<WorkspaceIdx> {
        let id = self.previous_workspace_id?;
        let internal_index = self.workspaces.iter().position(|w| w.id() == id)?;
        Some(self.to_external_index(internal_index))
    }

    pub fn iter(&self) -> Iter<Workspace<W>> {
        self.workspaces.iter()
    }

    pub fn iter_mut(&mut self) -> IterMut<Workspace<W>> {
        self.workspaces.iter_mut()
    }

    pub fn add_workspace_top(&mut self) {
        let ws = Workspace::new(
            self.output.clone(),
            self.clock.clone(),
            self.options.clone(),
        );
        self.push_front(ws);

        if let Some(switch) = &mut self.workspace_switch {
            switch.offset(1);
        }
    }

    pub fn add_workspace_bottom(&mut self) {
        let ws = Workspace::new(
            self.output.clone(),
            self.clock.clone(),
            self.options.clone(),
        );
        self.push_back(ws);
    }
}

impl<W: LayoutElement> IntoIterator for Workspaces<W> {
    type Item = Workspace<W>;
    type IntoIter = IntoIter<Workspace<W>>;
    fn into_iter(self) -> Self::IntoIter {
        self.workspaces.into_iter()
    }
}

impl<'a, W: LayoutElement> IntoIterator for &'a Workspaces<W> {
    type Item = &'a Workspace<W>;
    type IntoIter = Iter<'a, Workspace<W>>;
    fn into_iter(self) -> Self::IntoIter {
        self.workspaces.iter()
    }
}

impl<'a, W: LayoutElement> IntoIterator for &'a mut Workspaces<W> {
    type Item = &'a mut Workspace<W>;
    type IntoIter = IterMut<'a, Workspace<W>>;
    fn into_iter(self) -> Self::IntoIter {
        self.workspaces.iter_mut()
    }
}
