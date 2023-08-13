//! Window layout logic.
//!
//! Niri implements scrollable tiling with workspaces. There's one primary output, and potentially
//! multiple other outputs.
//!
//! Our layout has the following invariants:
//!
//! 1. Disconnecting and reconnecting the same output must not change the layout.
//!    * This includes both secondary outputs and the primary output.
//! 2. Connecting an output must not change the layout for any workspaces that were never on that
//!    output.
//!
//! Therefore, we implement the following logic: every workspace keeps track of which output it
//! originated on. When an output disconnects, its workspace (or workspaces, in case of the primary
//! output disconnecting) are appended to the (potentially new) primary output, but remember their
//! original output. Then, if the original output connects again, all workspaces originally from
//! there move back to that output.
//!
//! In order to avoid surprising behavior, if the user creates or moves any new windows onto a
//! workspace, it forgets its original output, and its current output becomes its original output.
//! Imagine a scenario: the user works with a laptop and a monitor at home, then takes their laptop
//! with them, disconnecting the monitor, and keeps working as normal, using the second monitor's
//! workspace just like any other. Then they come back, reconnect the second monitor, and now we
//! don't want an unassuming workspace to end up on it.
//!
//! ## Workspaces-only-on-primary considerations
//!
//! If this logic results in more than one workspace present on a secondary output, then as a
//! compromise we only keep the first workspace there, and move the rest to the primary output,
//! making the primary output their original output.

use std::cmp::{max, min};
use std::mem;
use std::time::Duration;

use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::space::SpaceElement;
use smithay::desktop::Window;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};
use smithay::wayland::compositor::{with_states, SurfaceData};
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

const PADDING: i32 = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputId(String);

pub trait LayoutElement: SpaceElement + PartialEq + Clone {
    fn request_size(&self, size: Size<i32, Logical>);
    fn send_pending_configure(&self);
    fn min_size(&self) -> Size<i32, Logical>;
    fn is_wl_surface(&self, wl_surface: &WlSurface) -> bool;
    fn send_frame<T, F>(
        &self,
        output: &Output,
        time: T,
        throttle: Option<Duration>,
        primary_scan_out_output: F,
    ) where
        T: Into<Duration>,
        F: FnMut(&WlSurface, &SurfaceData) -> Option<Output> + Copy;
}

#[derive(Debug)]
pub enum MonitorSet<W: LayoutElement> {
    /// At least one output is connected.
    Normal {
        /// Connected monitors.
        monitors: Vec<Monitor<W>>,
        /// Index of the primary monitor.
        primary_idx: usize,
        /// Index of the active monitor.
        active_monitor_idx: usize,
    },
    /// No outputs are connected, and these are the workspaces.
    NoOutputs(Vec<Workspace<W>>),
}

#[derive(Debug)]
pub struct Monitor<W: LayoutElement> {
    /// Output for this monitor.
    output: Output,
    // Must always contain at least one.
    workspaces: Vec<Workspace<W>>,
    /// Index of the currently active workspace.
    active_workspace_idx: usize,
}

#[derive(Debug)]
pub struct Workspace<W: LayoutElement> {
    /// The original output of this workspace.
    ///
    /// Most of the time this will be the workspace's current output, however, after an output
    /// disconnection, it may remain pointing to the disconnected output.
    original_output: OutputId,

    /// Current output of this workspace.
    output: Option<Output>,

    /// Latest known view size for this workspace.
    ///
    /// This should be computed from the current workspace output size, or, if all outputs have
    /// been disconnected, preserved until a new output is connected.
    view_size: Size<i32, Logical>,

    /// Columns of windows on this workspace.
    columns: Vec<Column<W>>,

    /// Index of the currently active column, if any.
    active_column_idx: usize,

    /// Offset of the view computed from the active column.
    view_offset: i32,
}

/// Width of a column.
#[derive(Debug, Clone, Copy)]
enum ColumnWidth {
    /// Proportion of the current view width.
    Proportion(f64),
    /// Fixed width in logical pixels.
    Fixed(i32),
}

#[derive(Debug)]
struct Column<W: LayoutElement> {
    /// Windows in this column.
    ///
    /// Must be non-empty.
    windows: Vec<W>,

    /// Index of the currently active window.
    active_window_idx: usize,

    /// Desired width of this column.
    width: ColumnWidth,
}

impl OutputId {
    pub fn new(output: &Output) -> Self {
        Self(output.name())
    }
}

impl LayoutElement for Window {
    fn request_size(&self, size: Size<i32, Logical>) {
        let toplevel = &self.toplevel();
        toplevel.with_pending_state(|state| {
            state.size = Some(size);
        });
        toplevel.send_pending_configure();
    }

    fn send_pending_configure(&self) {
        self.toplevel().send_pending_configure();
    }

    fn min_size(&self) -> Size<i32, Logical> {
        with_states(self.toplevel().wl_surface(), |state| {
            state
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .min_size
        })
    }

    fn is_wl_surface(&self, wl_surface: &WlSurface) -> bool {
        self.toplevel().wl_surface() == wl_surface
    }

    fn send_frame<T, F>(
        &self,
        output: &Output,
        time: T,
        throttle: Option<Duration>,
        primary_scan_out_output: F,
    ) where
        T: Into<Duration>,
        F: FnMut(&WlSurface, &SurfaceData) -> Option<Output> + Copy,
    {
        self.send_frame(output, time, throttle, primary_scan_out_output);
    }
}

impl ColumnWidth {
    fn resolve(self, view_width: i32) -> i32 {
        match self {
            ColumnWidth::Proportion(proportion) => (view_width as f64 * proportion).floor() as i32,
            ColumnWidth::Fixed(width) => width,
        }
    }
}

impl Default for ColumnWidth {
    fn default() -> Self {
        Self::Proportion(0.5)
    }
}

impl<W: LayoutElement> MonitorSet<W> {
    pub fn new() -> Self {
        Self::NoOutputs(vec![])
    }

    pub fn add_output(&mut self, output: Output) {
        let id = OutputId::new(&output);

        *self = match mem::take(self) {
            MonitorSet::Normal {
                mut monitors,
                primary_idx,
                active_monitor_idx,
            } => {
                let primary = &mut monitors[primary_idx];

                let mut workspaces = vec![];
                for i in (0..primary.workspaces.len()).rev() {
                    if primary.workspaces[i].original_output == id {
                        let ws = primary.workspaces.remove(i);
                        workspaces.push(ws);
                    }
                }
                workspaces.reverse();
                if workspaces.iter().all(|ws| ws.has_windows()) {
                    // Make sure there's always an empty workspace.
                    workspaces.push(Workspace::new(output.clone()));
                }

                for ws in &mut workspaces {
                    ws.set_output(Some(output.clone()));
                }

                monitors.push(Monitor {
                    output,
                    workspaces,
                    active_workspace_idx: 0,
                });
                MonitorSet::Normal {
                    monitors,
                    primary_idx,
                    active_monitor_idx,
                }
            }
            MonitorSet::NoOutputs(mut workspaces) => {
                // We know there are no empty workspaces there, so add one.
                workspaces.push(Workspace::new(output.clone()));

                for workspace in &mut workspaces {
                    workspace.set_output(Some(output.clone()));
                }

                let monitor = Monitor {
                    output,
                    workspaces,
                    active_workspace_idx: 0,
                };
                MonitorSet::Normal {
                    monitors: vec![monitor],
                    primary_idx: 0,
                    active_monitor_idx: 0,
                }
            }
        }
    }

    pub fn remove_output(&mut self, output: &Output) {
        *self = match mem::take(self) {
            MonitorSet::Normal {
                mut monitors,
                mut primary_idx,
                mut active_monitor_idx,
            } => {
                let idx = monitors
                    .iter()
                    .position(|mon| &mon.output == output)
                    .expect("trying to remove non-existing output");
                let monitor = monitors.remove(idx);
                let mut workspaces = monitor.workspaces;

                for ws in &mut workspaces {
                    ws.set_output(None);
                }

                // Get rid of empty workspaces.
                workspaces.retain(|ws| ws.has_windows());

                if monitors.is_empty() {
                    // Removed the last monitor.
                    MonitorSet::NoOutputs(workspaces)
                } else {
                    if primary_idx >= idx {
                        // Update primary_idx to either still point at the same monitor, or at some
                        // other monitor if the primary has been removed.
                        primary_idx = primary_idx.saturating_sub(1);
                    }
                    if active_monitor_idx >= idx {
                        // Update active_monitor_idx to either still point at the same monitor, or
                        // at some other monitor if the active monitor has
                        // been removed.
                        active_monitor_idx = active_monitor_idx.saturating_sub(1);
                    }

                    let primary = &mut monitors[primary_idx];
                    for ws in &mut workspaces {
                        ws.set_output(Some(primary.output.clone()));
                    }

                    let empty = primary.workspaces.remove(primary.workspaces.len() - 1);
                    primary.workspaces.extend(workspaces);
                    primary.workspaces.push(empty);

                    MonitorSet::Normal {
                        monitors,
                        primary_idx,
                        active_monitor_idx,
                    }
                }
            }
            MonitorSet::NoOutputs(_) => {
                panic!("tried to remove output when there were already none")
            }
        }
    }

    pub fn add_window(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        window: W,
        activate: bool,
    ) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = self
        else {
            panic!()
        };

        let monitor = &mut monitors[monitor_idx];
        let workspace = &mut monitor.workspaces[workspace_idx];

        if activate {
            *active_monitor_idx = monitor_idx;
            monitor.active_workspace_idx = workspace_idx;
            // Configure will be sent in add_window().
            window.set_activate(true);
        }

        workspace.add_window(window.clone(), activate);

        if workspace_idx == monitor.workspaces.len() - 1 {
            // Insert a new empty workspace.
            let ws = Workspace::new(monitor.output.clone());
            monitor.workspaces.push(ws);
        }
    }

    pub fn add_window_to_output(&mut self, output: &Output, window: W, activate: bool) {
        let MonitorSet::Normal { monitors, .. } = self else {
            panic!()
        };

        let (monitor_idx, monitor) = monitors
            .iter()
            .enumerate()
            .find(|(_, mon)| &mon.output == output)
            .unwrap();
        let workspace_idx = monitor.active_workspace_idx;

        self.add_window(monitor_idx, workspace_idx, window, activate);
    }

    pub fn remove_window(&mut self, window: &W) {
        match self {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for (idx, ws) in mon.workspaces.iter_mut().enumerate() {
                        if ws.has_window(window) {
                            ws.remove_window(window);

                            // Clean up empty workspaces that are not active and not last.
                            if !ws.has_windows()
                                && idx != mon.active_workspace_idx
                                && idx != mon.workspaces.len() - 1
                            {
                                mon.workspaces.remove(idx);
                            }

                            break;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs(workspaces) => {
                for (idx, ws) in workspaces.iter_mut().enumerate() {
                    if ws.has_window(window) {
                        ws.remove_window(window);

                        // Clean up empty workspaces.
                        if !ws.has_windows() {
                            workspaces.remove(idx);
                        }

                        break;
                    }
                }
            }
        }
    }

    pub fn update_window(&mut self, window: &W) {
        match self {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.update_window(window);
                            break;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs(workspaces) => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.update_window(window);
                        break;
                    }
                }
            }
        }
    }

    pub fn send_frame(&self, output: &Output, time: Duration) {
        if let MonitorSet::Normal { monitors, .. } = self {
            for mon in monitors {
                if &mon.output == output {
                    mon.workspaces[mon.active_workspace_idx].send_frame(time);
                }
            }
        }
    }

    pub fn find_window_and_output(&mut self, wl_surface: &WlSurface) -> Option<(W, Output)> {
        if let MonitorSet::Normal { monitors, .. } = self {
            for mon in monitors {
                for ws in &mut mon.workspaces {
                    if let Some(window) = ws.find_wl_surface(wl_surface) {
                        return Some((window.clone(), mon.output.clone()));
                    }
                }
            }
        }

        None
    }

    pub fn update_output(&mut self, output: &Output) {
        let MonitorSet::Normal { monitors, .. } = self else {
            panic!()
        };

        for mon in monitors {
            if &mon.output == output {
                for ws in &mut mon.workspaces {
                    ws.set_view_size(output_size(output));
                }
                break;
            }
        }
    }

    pub fn activate_window(&mut self, window: &W) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = self
        else {
            todo!()
        };

        for (monitor_idx, mon) in monitors.iter_mut().enumerate() {
            for (workspace_idx, ws) in mon.workspaces.iter_mut().enumerate() {
                if ws.has_window(window) {
                    *active_monitor_idx = monitor_idx;
                    mon.active_workspace_idx = workspace_idx;
                    ws.activate_window(window);
                    break;
                }
            }
        }
    }

    pub fn activate_output(&mut self, output: &Output) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = self
        else {
            return;
        };

        let idx = monitors
            .iter()
            .position(|mon| &mon.output == output)
            .unwrap();
        *active_monitor_idx = idx;
    }

    pub fn active_output(&self) -> Option<&Output> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = self
        else {
            return None;
        };

        Some(&monitors[*active_monitor_idx].output)
    }

    fn active_workspace(&mut self) -> Option<&mut Workspace<W>> {
        let monitor = self.active_monitor()?;
        Some(&mut monitor.workspaces[monitor.active_workspace_idx])
    }

    fn active_monitor(&mut self) -> Option<&mut Monitor<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = self
        else {
            return None;
        };

        Some(&mut monitors[*active_monitor_idx])
    }

    pub fn move_left(&mut self) {
        let Some(workspace) = self.active_workspace() else {
            return;
        };
        workspace.move_left();
    }

    pub fn move_right(&mut self) {
        let Some(workspace) = self.active_workspace() else {
            return;
        };
        workspace.move_right();
    }

    pub fn move_down(&mut self) {
        let Some(workspace) = self.active_workspace() else {
            return;
        };
        workspace.move_down();
    }

    pub fn move_up(&mut self) {
        let Some(workspace) = self.active_workspace() else {
            return;
        };
        workspace.move_up();
    }

    pub fn focus_left(&mut self) {
        let Some(workspace) = self.active_workspace() else {
            return;
        };
        workspace.focus_left();
    }

    pub fn focus_right(&mut self) {
        let Some(workspace) = self.active_workspace() else {
            return;
        };
        workspace.focus_right();
    }

    pub fn focus_down(&mut self) {
        let Some(workspace) = self.active_workspace() else {
            return;
        };
        workspace.focus_down();
    }

    pub fn focus_up(&mut self) {
        let Some(workspace) = self.active_workspace() else {
            return;
        };
        workspace.focus_up();
    }

    pub fn move_to_workspace_up(&mut self) {
        let MonitorSet::Normal {
            monitors,
            ref active_monitor_idx,
            ..
        } = self
        else {
            return;
        };

        let monitor = &mut monitors[*active_monitor_idx];
        let source_workspace_idx = monitor.active_workspace_idx;

        let new_idx = source_workspace_idx.saturating_sub(1);
        if new_idx == source_workspace_idx {
            return;
        }

        let workspace = &mut monitor.workspaces[source_workspace_idx];
        if workspace.columns.is_empty() {
            return;
        }

        let column = &mut workspace.columns[workspace.active_column_idx];
        let window = column.windows[column.active_window_idx].clone();
        workspace.remove_window(&window);

        if !workspace.has_windows() && source_workspace_idx != monitor.workspaces.len() - 1 {
            monitor.workspaces.remove(source_workspace_idx);
        }

        self.add_window(*active_monitor_idx, new_idx, window, true);
    }

    pub fn move_to_workspace_down(&mut self) {
        let MonitorSet::Normal {
            monitors,
            ref active_monitor_idx,
            ..
        } = self
        else {
            return;
        };

        let monitor = &mut monitors[*active_monitor_idx];
        let source_workspace_idx = monitor.active_workspace_idx;

        let mut new_idx = min(source_workspace_idx + 1, monitor.workspaces.len() - 1);
        if new_idx == source_workspace_idx {
            return;
        }

        let workspace = &mut monitor.workspaces[source_workspace_idx];
        if workspace.columns.is_empty() {
            return;
        }

        let column = &mut workspace.columns[workspace.active_column_idx];
        let window = column.windows[column.active_window_idx].clone();
        workspace.remove_window(&window);

        if !workspace.has_windows() {
            monitor.workspaces.remove(source_workspace_idx);
            new_idx -= 1;
        }

        self.add_window(*active_monitor_idx, new_idx, window, true);
    }

    pub fn switch_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };

        let source_workspace_idx = monitor.active_workspace_idx;

        let new_idx = source_workspace_idx.saturating_sub(1);
        if new_idx == source_workspace_idx {
            return;
        }

        monitor.active_workspace_idx = new_idx;

        if !monitor.workspaces[source_workspace_idx].has_windows()
            && source_workspace_idx != monitor.workspaces.len() - 1
        {
            monitor.workspaces.remove(source_workspace_idx);
        }
    }

    pub fn switch_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };

        let source_workspace_idx = monitor.active_workspace_idx;

        let mut new_idx = min(source_workspace_idx + 1, monitor.workspaces.len() - 1);
        if new_idx == source_workspace_idx {
            return;
        }

        if !monitor.workspaces[source_workspace_idx].has_windows() {
            monitor.workspaces.remove(source_workspace_idx);
            new_idx -= 1;
        }

        monitor.active_workspace_idx = new_idx;
    }

    pub fn consume_into_column(&mut self) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = self
        else {
            return;
        };

        let monitor = &mut monitors[*active_monitor_idx];

        let workspace = &mut monitor.workspaces[monitor.active_workspace_idx];
        workspace.consume_into_column();
    }

    pub fn expel_from_column(&mut self) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = self
        else {
            return;
        };

        let monitor = &mut monitors[*active_monitor_idx];
        let workspace = &mut monitor.workspaces[monitor.active_workspace_idx];
        workspace.expel_from_column();
    }

    pub fn focus(&self) -> Option<&W> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = self
        else {
            return None;
        };

        let monitor = &monitors[*active_monitor_idx];
        let workspace = &monitor.workspaces[monitor.active_workspace_idx];
        if !workspace.has_windows() {
            return None;
        }

        let column = &workspace.columns[workspace.active_column_idx];
        Some(&column.windows[column.active_window_idx])
    }

    pub fn workspace_for_output(&self, output: &Output) -> Option<&Workspace<W>> {
        let MonitorSet::Normal { monitors, .. } = self else {
            return None;
        };

        monitors.iter().find_map(|monitor| {
            if &monitor.output == output {
                Some(&monitor.workspaces[monitor.active_workspace_idx])
            } else {
                None
            }
        })
    }

    pub fn window_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(&W, Point<i32, Logical>)> {
        let ws = self.workspace_for_output(output).unwrap();
        ws.window_under(pos_within_output)
    }

    /// Refreshes the `Workspace`s.
    pub fn refresh(&mut self) {
        // TODO
    }

    fn verify_invariants(&self) {
        let (monitors, &primary_idx, &active_monitor_idx) = match &self {
            MonitorSet::Normal {
                monitors,
                primary_idx,
                active_monitor_idx,
            } => (monitors, primary_idx, active_monitor_idx),
            MonitorSet::NoOutputs(workspaces) => {
                for workspace in workspaces {
                    assert!(
                        !workspace.has_windows(),
                        "with no outputs there cannot be empty workspaces"
                    );

                    workspace.verify_invariants();
                }

                return;
            }
        };

        assert!(primary_idx <= monitors.len());
        assert!(active_monitor_idx <= monitors.len());

        for (idx, monitor) in monitors.iter().enumerate() {
            assert!(
                !monitor.workspaces.is_empty(),
                "monitor monitor must have at least one workspace"
            );

            let monitor_id = OutputId::new(&monitor.output);

            if idx == primary_idx {
            } else {
                assert!(
                    monitor
                        .workspaces
                        .iter()
                        .any(|workspace| workspace.original_output == monitor_id),
                    "secondary monitor must have all own workspaces"
                );
            }

            // FIXME: verify that primary doesn't have any workspaces for which their own monitor
            // exists.

            for workspace in &monitor.workspaces {
                workspace.verify_invariants();
            }
        }
    }
}

impl MonitorSet<Window> {
    pub fn render_elements(
        &self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
        let ws = self.workspace_for_output(output).unwrap();
        ws.render_elements(renderer)
    }
}

impl<W: LayoutElement> Default for MonitorSet<W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W: LayoutElement> Workspace<W> {
    fn new(output: Output) -> Self {
        Self {
            original_output: OutputId::new(&output),
            view_size: output_size(&output),
            output: Some(output),
            columns: vec![],
            active_column_idx: 0,
            view_offset: 0,
        }
    }

    fn refresh(&self) {
        // FIXME: proper overlap.
    }

    fn windows(&self) -> impl Iterator<Item = &W> + '_ {
        self.columns.iter().flat_map(|col| col.windows.iter())
    }

    fn set_output(&mut self, output: Option<Output>) {
        if self.output == output {
            return;
        }

        if let Some(output) = self.output.take() {
            for win in self.windows() {
                win.output_leave(&output);
            }
        }

        if let Some(output) = output {
            self.set_view_size(output_size(&output));

            self.output = Some(output);

            for win in self.windows() {
                self.enter_output_for_window(win);
            }
        }
    }

    fn enter_output_for_window(&self, window: &W) {
        if let Some(output) = &self.output {
            // FIXME: proper overlap.
            window.output_enter(
                output,
                Rectangle::from_loc_and_size((0, 0), (i32::MAX, i32::MAX)),
            );
        }
    }

    fn set_view_size(&mut self, size: Size<i32, Logical>) {
        if self.view_size == size {
            return;
        }

        self.view_size = size;
        for col in &mut self.columns {
            col.update_window_sizes(self.view_size);
        }
    }

    fn has_windows(&self) -> bool {
        self.windows().next().is_some()
    }

    fn has_window(&self, window: &W) -> bool {
        self.windows().any(|win| win == window)
    }

    fn find_wl_surface(&self, wl_surface: &WlSurface) -> Option<&W> {
        self.windows().find(|win| win.is_wl_surface(wl_surface))
    }

    /// Computes the X position of the windows in the given column, in logical coordinates.
    fn column_x(&self, column_idx: usize) -> i32 {
        let mut x = PADDING;

        for column in self.columns.iter().take(column_idx) {
            x += column.size().w + PADDING;
        }

        x
    }

    fn add_window(&mut self, window: W, activate: bool) {
        self.enter_output_for_window(&window);
        // Configure will be sent in Column::new().
        window.set_activate(activate);

        if activate {
            for win in self.windows() {
                win.set_activate(false);
                win.send_pending_configure();
            }
        }

        let idx = if self.columns.is_empty() {
            0
        } else {
            self.active_column_idx + 1
        };

        let column = Column::new(window, self.view_size);
        self.columns.insert(idx, column);

        if activate {
            self.active_column_idx = idx;
        }
    }

    fn remove_window(&mut self, window: &W) {
        if let Some(output) = &self.output {
            window.output_leave(output);
        }

        let column_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();
        let column = &mut self.columns[column_idx];

        let window_idx = column.windows.iter().position(|win| win == window).unwrap();
        column.windows.remove(window_idx);
        if column.windows.is_empty() {
            self.columns.remove(column_idx);
            if self.columns.is_empty() {
                return;
            }

            self.active_column_idx = min(self.active_column_idx, self.columns.len() - 1);
            let column = &self.columns[self.active_column_idx];
            let window = &column.windows[column.active_window_idx];
            window.set_activate(true);
            window.send_pending_configure();
            return;
        }

        column.active_window_idx = min(column.active_window_idx, column.windows.len() - 1);
        if self.active_column_idx == column_idx {
            let window = &column.windows[column.active_window_idx];
            window.set_activate(true);
        }
        column.update_window_sizes(self.view_size);
    }

    fn update_window(&mut self, window: &W) {
        let column = self
            .columns
            .iter_mut()
            .find(|col| col.contains(window))
            .unwrap();
        column.update_window_sizes(self.view_size);
    }

    fn activate_window(&mut self, window: &W) {
        for win in self.windows() {
            if win != window {
                win.set_activate(false);
                win.send_pending_configure();
            }
        }
        window.set_activate(true);
        window.send_pending_configure();

        let column_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();
        let column = &mut self.columns[column_idx];

        column.activate_window(window);
        self.active_column_idx = column_idx;
    }

    fn verify_invariants(&self) {
        assert!(self.view_size.w > 0);
        assert!(self.view_size.h > 0);

        assert!(self.columns.is_empty() || self.active_column_idx < self.columns.len());

        for column in &self.columns {
            column.verify_invariants();
        }
    }

    fn focus_left(&mut self) {
        let new_idx = self.active_column_idx.saturating_sub(1);
        if self.active_column_idx == new_idx {
            return;
        }

        let column = &self.columns[self.active_column_idx];
        let window = &column.windows[column.active_window_idx];
        window.set_activate(false);
        window.send_pending_configure();

        self.active_column_idx = new_idx;

        let column = &self.columns[self.active_column_idx];
        let window = &column.windows[column.active_window_idx];
        window.set_activate(true);
        window.send_pending_configure();
    }

    fn focus_right(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let new_idx = min(self.active_column_idx + 1, self.columns.len() - 1);
        if self.active_column_idx == new_idx {
            return;
        }

        let column = &self.columns[self.active_column_idx];
        let window = &column.windows[column.active_window_idx];
        window.set_activate(false);
        window.send_pending_configure();

        self.active_column_idx = new_idx;

        let column = &self.columns[self.active_column_idx];
        let window = &column.windows[column.active_window_idx];
        window.set_activate(true);
        window.send_pending_configure();
    }

    fn focus_down(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.columns[self.active_column_idx].focus_down();
    }

    fn focus_up(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.columns[self.active_column_idx].focus_up();
    }

    fn move_left(&mut self) {
        let new_idx = self.active_column_idx.saturating_sub(1);
        if self.active_column_idx == new_idx {
            return;
        }

        self.columns.swap(self.active_column_idx, new_idx);
        self.active_column_idx = new_idx;
    }

    fn move_right(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let new_idx = min(self.active_column_idx + 1, self.columns.len() - 1);
        if self.active_column_idx == new_idx {
            return;
        }

        self.columns.swap(self.active_column_idx, new_idx);
        self.active_column_idx = new_idx;
    }

    fn move_down(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.columns[self.active_column_idx].move_down();
    }

    fn move_up(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.columns[self.active_column_idx].move_up();
    }

    fn consume_into_column(&mut self) {
        if self.columns.len() < 2 {
            return;
        }

        if self.active_column_idx == self.columns.len() - 1 {
            return;
        }

        let source_column_idx = self.active_column_idx + 1;

        let source_column = &mut self.columns[source_column_idx];
        let window = source_column.windows[0].clone();
        self.remove_window(&window);

        let target_column = &mut self.columns[self.active_column_idx];
        target_column.add_window(self.view_size, window);
    }

    fn expel_from_column(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let source_column = &mut self.columns[self.active_column_idx];
        if source_column.windows.len() == 1 {
            return;
        }

        let window = source_column.windows[source_column.active_window_idx].clone();
        self.remove_window(&window);

        self.add_window(window, true);
    }

    fn send_frame(&self, time: Duration) {
        let output = self.output.as_ref().unwrap();
        for win in self.windows() {
            win.send_frame(output, time, None, |_, _| Some(output.clone()));
        }
    }

    fn view_pos(&self) -> i32 {
        self.column_x(self.active_column_idx) + self.view_offset - PADDING
    }

    fn window_under(
        &self,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(&W, Point<i32, Logical>)> {
        let view_pos = self.view_pos();

        let mut pos = pos_within_output;
        pos.x += view_pos as f64;

        let mut x = PADDING;
        for col in &self.columns {
            let mut y = PADDING;

            for win in &col.windows {
                let geom = win.geometry();

                // x, y point at the top-left of the window geometry.
                let win_pos = Point::from((x, y)) - geom.loc;
                if win.is_in_input_region(&(pos - win_pos.to_f64())) {
                    let mut win_pos_within_output = win_pos;
                    win_pos_within_output.x -= view_pos;
                    return Some((win, win_pos_within_output));
                }

                y += geom.size.h + PADDING;
            }

            x += col.size().w + PADDING;
        }

        None
    }
}

impl Workspace<Window> {
    fn render_elements(
        &self,
        renderer: &mut GlesRenderer,
    ) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
        let mut rv = vec![];
        let view_pos = self.view_pos();

        let mut x = PADDING;
        for col in &self.columns {
            let mut y = PADDING;

            for win in &col.windows {
                let geom = win.geometry();

                let win_pos = Point::from((x - view_pos, y)) - geom.loc;
                rv.extend(win.render_elements(
                    renderer,
                    win_pos.to_physical(1),
                    Scale::from(1.),
                    1.,
                ));
                y += win.geometry().size.h + PADDING;
            }

            x += col.size().w + PADDING;
        }

        rv
    }
}

impl<W: LayoutElement> Column<W> {
    fn new(window: W, view_size: Size<i32, Logical>) -> Self {
        let mut rv = Self {
            windows: vec![],
            active_window_idx: 0,
            width: ColumnWidth::default(),
        };

        rv.add_window(view_size, window);

        rv
    }

    fn window_count(&self) -> usize {
        self.windows.len()
    }

    fn set_width(&mut self, view_size: Size<i32, Logical>, width: ColumnWidth) {
        self.width = width;
        self.update_window_sizes(view_size);
    }

    fn contains(&self, window: &W) -> bool {
        self.windows.iter().any(|win| win == window)
    }

    fn activate_window(&mut self, window: &W) {
        let idx = self.windows.iter().position(|win| win == window).unwrap();
        self.active_window_idx = idx;
    }

    fn add_window(&mut self, view_size: Size<i32, Logical>, window: W) {
        self.windows.push(window);
        self.update_window_sizes(view_size);
    }

    fn update_window_sizes(&mut self, view_size: Size<i32, Logical>) {
        let min_width = self
            .windows
            .iter()
            .filter_map(|win| {
                let w = win.min_size().w;
                if w == 0 {
                    None
                } else {
                    Some(w)
                }
            })
            .max()
            .unwrap_or(1);
        let width = self.width.resolve(view_size.w - PADDING) - PADDING;
        let height = (view_size.h - PADDING) / self.window_count() as i32 - PADDING;
        let size = Size::from((max(width, min_width), max(height, 1)));

        for win in &self.windows {
            win.request_size(size);
        }
    }

    /// Computes the size of the column including top and bottom padding.
    fn size(&self) -> Size<i32, Logical> {
        let mut total = Size::from((0, PADDING));

        for window in &self.windows {
            let size = window.geometry().size;
            total.w = max(total.w, size.w);
            total.h += size.h + PADDING;
        }

        total
    }

    fn focus_up(&mut self) {
        let new_idx = self.active_window_idx.saturating_sub(1);
        if self.active_window_idx == new_idx {
            return;
        }

        self.windows[self.active_window_idx].set_activate(false);
        self.windows[self.active_window_idx].send_pending_configure();
        self.windows[new_idx].set_activate(true);
        self.windows[new_idx].send_pending_configure();
        self.active_window_idx = new_idx;
    }

    fn focus_down(&mut self) {
        let new_idx = min(self.active_window_idx + 1, self.windows.len() - 1);
        if self.active_window_idx == new_idx {
            return;
        }

        self.windows[self.active_window_idx].set_activate(false);
        self.windows[self.active_window_idx].send_pending_configure();
        self.windows[new_idx].set_activate(true);
        self.windows[new_idx].send_pending_configure();
        self.active_window_idx = new_idx;
    }

    fn move_up(&mut self) {
        let new_idx = self.active_window_idx.saturating_sub(1);
        if self.active_window_idx == new_idx {
            return;
        }

        self.windows.swap(self.active_window_idx, new_idx);
        self.active_window_idx = new_idx;
    }

    fn move_down(&mut self) {
        let new_idx = min(self.active_window_idx + 1, self.windows.len() - 1);
        if self.active_window_idx == new_idx {
            return;
        }

        self.windows.swap(self.active_window_idx, new_idx);
        self.active_window_idx = new_idx;
    }

    fn verify_invariants(&self) {
        assert!(!self.windows.is_empty(), "columns can't be empty");
        assert!(self.active_window_idx < self.windows.len());
    }
}

pub fn output_size(output: &Output) -> Size<i32, Logical> {
    let output_scale = output.current_scale().integer_scale();
    let output_transform = output.current_transform();
    let output_mode = output.current_mode().unwrap();

    output_transform
        .transform_size(output_mode.size)
        .to_logical(output_scale)
}

pub fn configure_new_window(view_size: Size<i32, Logical>, window: &Window) {
    let width = ColumnWidth::default().resolve(view_size.w - PADDING) - PADDING;
    let height = view_size.h - PADDING * 2;
    let size = Size::from((max(width, 1), max(height, 1)));

    let bounds = Size::from((view_size.w - PADDING * 2, view_size.h - PADDING * 2));

    window.toplevel().with_pending_state(|state| {
        state.size = Some(size);
        state.bounds = Some(bounds);
    });
}
