use std::collections::VecDeque;
use std::ops::{Index, IndexMut};

use super::monitor::WorkspaceSwitch;
use super::workspace::{self, OutputId, Workspace, WorkspaceId, WorkspaceRenderElement};
use super::{LayoutElement, Options};

#[derive(Debug)]
pub struct Workspaces<W: LayoutElement> {
    workspaces: VecDeque<Workspace<W>>,
    active_workspace_idx: isize,
    workspace_idx_offset: usize,
    previous_workspace_id: Option<WorkspaceId>,
    workspace_switch: Option<WorkspaceSwitch>,
    empty_workspace_above_first: bool,
}

impl<W: LayoutElement> Index<isize> for Workspaces<W> {
    type Output = Workspace<W>;

    fn index(&self, index: isize) -> &Self::Output {
        &self.workspaces[self.to_internal_index(index)]
    }
}

impl<W: LayoutElement> IndexMut<isize> for Workspaces<W> {
    fn index_mut(&mut self, index: isize) -> &mut Self::Output {
        &mut self.workspaces[self.to_internal_index(index)]
    }
}

impl<W: LayoutElement> Workspaces<W> {
    pub fn new(workspaces: Vec<Workspace<W>>, empty_workspace_above_first: bool) -> Self {
        Self {
            workspaces: workspaces.into(),
            active_workspace_idx: 0,
            workspace_idx_offset: 0,
            previous_workspace_id: None,
            workspace_switch: None,
            empty_workspace_above_first,
        }
    }

    #[inline]
    fn to_internal_index(&self, external_index: isize) -> usize {
        debug_assert!(external_index + self.workspace_idx_offset as isize > 0);
        (external_index + self.workspace_idx_offset as isize) as usize
    }

    #[inline]
    fn to_external_index(&self, internal_index: usize) -> isize {
        internal_index as isize - self.workspace_idx_offset as isize
    }

    #[inline]
    fn internal_active_workspace_idx(&self) -> usize {
        self.to_internal_index(self.active_workspace_idx)
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
    pub fn get(&self, index: isize) -> Option<&Workspace<W>> {
        self.workspaces.get(self.to_internal_index(index))
    }

    #[inline]
    pub fn get_mut(&mut self, index: isize) -> Option<&mut Workspace<W>> {
        self.workspaces.get_mut(self.to_internal_index(index))
    }

    pub fn clean_up_workspaces(&mut self, empty_workspace_above_first: bool) {
        assert!(self.workspace_switch.is_none());

        let range_start = if empty_workspace_above_first { 1 } else { 0 };
        for internal_idx in (range_start..self.workspaces.len() - 1).rev() {
            if self.internal_active_workspace_idx() == internal_idx {
                continue;
            }

            if !self.workspaces[internal_idx].has_windows_or_name() {
                self.workspaces.remove(internal_idx);
                if self.internal_active_workspace_idx() > internal_idx {
                    self.active_workspace_idx -= 1;
                }
            }
        }

        // Special case handling when empty_workspace_above_first is set and all workspaces
        // are empty.
        if empty_workspace_above_first && self.workspaces.len() == 2 {
            assert!(!self.workspaces[0].has_windows_or_name());
            assert!(!self.workspaces[1].has_windows_or_name());
            self.workspaces.remove(1);
            self.active_workspace_idx = 0;
        }
    }

    #[inline]
    pub fn active_workspace_idx(&self) -> isize {
        self.active_workspace_idx
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

    pub fn activate_workspace(&mut self, idx: isize) {
        // also include to update previous workspace id
        todo!()
    }

    pub fn previous_workspace_idx(&self) -> Option<isize> {
        let id = self.previous_workspace_id?;
        let internal_index = self.workspaces.iter().position(|w| w.id() == id)?;
        Some(self.to_external_index(internal_index))
    }
}
