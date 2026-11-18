use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ratatui::widgets::ListState;

use crate::config::{Config, MountMode};
use crate::disk::{self, BlockDevice};
use crate::mount;
use crate::runner::{CommandRunner, CommandSpec};

/// Number of rows jumped by PageUp / PageDown.
const PAGE_STEP: usize = 10;

/// Maximum number of rows the details pane can be scrolled down (`]`).
const DETAILS_SCROLL_MAX: u16 = 100;

/// `lsblk` device type of loop devices (snap images, mounted ISO files, ...).
const LOOP_DEVICE_TYPE: &str = "loop";

/// Prefix of the status line asking for the second `f` press. [`App::clear_force_confirm`]
/// uses it to remove the prompt again when the pending confirmation is dropped.
const FORCE_CONFIRM_PROMPT: &str = "Press f again to force-unmount";

/// Represents a flattened view item in the TUI list
#[derive(Debug, Clone)]
pub struct DeviceItem {
    pub device: BlockDevice,
    pub depth: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionStatus {
    None,
    Mounting,
    Unmounting,
    Success(String),
    Error(String),
}

/// What kind of operation an [`InteractiveRequest`] performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveOp {
    Mount,
    Unmount,
    ForceUnmount,
}

/// A command that needs the real terminal (sudo password prompt).
///
/// The App never touches terminal state: it only describes the request. `main`
/// suspends the TUI, runs it with inherited stdio and reports back through
/// [`App::finish`].
#[derive(Debug, Clone, PartialEq)]
pub struct InteractiveRequest {
    pub program: String,
    pub args: Vec<String>,
    pub op: InteractiveOp,
    pub device_name: String,
    /// Human readable context from the failed non-interactive attempt
    /// (usually `udisksctl`'s stderr).
    pub context: String,
    /// Optional second attempt, used by force unmount (device path → mount point).
    pub fallback_args: Option<Vec<String>>,
    /// Directory `main` has to create right before handing the terminal over
    /// (sudo mode only). The App only describes it, it never creates anything.
    pub mount_target: Option<PathBuf>,
}

pub struct App {
    pub devices: Vec<BlockDevice>,
    /// Effective mount configuration (mode + root) loaded at startup.
    pub config: Config,
    /// Collapsed device names (not indices) so the state survives refreshes.
    pub collapsed: HashSet<String>,
    pub list_state: ListState,
    pub action: ActionStatus,
    /// Device with an armed force-unmount confirmation: the first `f` arms it,
    /// the second `f` on the same device actually runs the command.
    pub force_confirm: Option<String>,
    /// Hide `loop` devices (snap images, ...) in the list; `l` toggles it.
    pub hide_loops: bool,
    /// Search query (already applied while typing); empty means "no filter".
    pub search: String,
    /// `true` while the `/` input line has the keyboard: every printable key
    /// belongs to the query, global shortcuts are suspended.
    pub search_editing: bool,
    /// First details row shown by the details pane (`[` / `]`).
    pub details_scroll: u16,
    pub running: bool,
}

impl App {
    /// Build an app with the default configuration (udisksctl mode).
    ///
    /// Only the unit tests build an app without a loaded configuration; the
    /// real entry point always goes through [`App::with_config`].
    #[cfg(test)]
    pub fn new(devices: Vec<BlockDevice>) -> Self {
        Self::with_config(devices, Config::default())
    }

    pub fn with_config(devices: Vec<BlockDevice>, config: Config) -> Self {
        let mut app = Self {
            devices,
            config,
            collapsed: HashSet::new(),
            list_state: ListState::default(),
            action: ActionStatus::None,
            force_confirm: None,
            hide_loops: true,
            search: String::new(),
            search_editing: false,
            details_scroll: 0,
            running: true,
        };
        app.clamp_selection();
        app
    }

    /// Build a flat list for display: every device recursively, children only
    /// when their parent is expanded.
    ///
    /// Two view filters are applied on the way down:
    ///
    /// * loop devices are dropped when [`App::hide_loops`] is set (with their
    ///   whole subtree, a loop device cannot be mounted/unmounted here);
    /// * a non-empty search query keeps only the matching devices, the branches
    ///   leading to them and (for a matching device) its whole subtree. While a
    ///   query is active the collapse state is ignored so matches are visible.
    pub fn flat_items(&self) -> Vec<DeviceItem> {
        let query = self.search_query();
        let mut items = Vec::new();
        for device in &self.devices {
            Self::collect_items(
                &mut items,
                device,
                0,
                &self.collapsed,
                self.hide_loops,
                query.as_deref(),
            );
        }
        items
    }

    /// Normalised search query: `None` when nothing has to be filtered.
    ///
    /// Surrounding whitespace is ignored (a query of blanks is "no filter"), and
    /// matching is case-insensitive, so the query is lowercased once here.
    fn search_query(&self) -> Option<String> {
        let query = self.search.trim();
        if query.is_empty() {
            None
        } else {
            Some(query.to_lowercase())
        }
    }

    /// `true` when a search query filters the list.
    pub fn filter_active(&self) -> bool {
        self.search_query().is_some()
    }

    fn collect_items(
        items: &mut Vec<DeviceItem>,
        device: &BlockDevice,
        depth: usize,
        collapsed: &HashSet<String>,
        hide_loops: bool,
        query: Option<&str>,
    ) {
        if hide_loops && is_loop(device) {
            return;
        }

        let Some(query) = query else {
            // No query: the plain tree, respecting the collapse state.
            items.push(DeviceItem {
                device: device.clone(),
                depth,
            });
            if collapsed.contains(&device.name) {
                return;
            }
            if let Some(children) = &device.children {
                for child in children {
                    Self::collect_items(items, child, depth + 1, collapsed, hide_loops, None);
                }
            }
            return;
        };

        // A matching device is kept together with everything below it.
        if device_matches_query(device, query) {
            Self::push_subtree(items, device, depth, hide_loops);
            return;
        }

        // Only descendants match: this device stays as their branch. Siblings
        // without a match are dropped, an unmatched leaf disappears entirely.
        let mut descendants = Vec::new();
        if let Some(children) = &device.children {
            for child in children {
                Self::collect_items(
                    &mut descendants,
                    child,
                    depth + 1,
                    collapsed,
                    hide_loops,
                    Some(query),
                );
            }
        }
        if descendants.is_empty() {
            return;
        }
        items.push(DeviceItem {
            device: device.clone(),
            depth,
        });
        items.extend(descendants);
    }

    /// Push a device and its whole subtree (loop devices still filtered).
    fn push_subtree(
        items: &mut Vec<DeviceItem>,
        device: &BlockDevice,
        depth: usize,
        hide_loops: bool,
    ) {
        if hide_loops && is_loop(device) {
            return;
        }
        items.push(DeviceItem {
            device: device.clone(),
            depth,
        });
        if let Some(children) = &device.children {
            for child in children {
                Self::push_subtree(items, child, depth + 1, hide_loops);
            }
        }
    }

    pub fn has_children(device: &BlockDevice) -> bool {
        device
            .children
            .as_ref()
            .is_some_and(|children| !children.is_empty())
    }

    pub fn is_collapsed(&self, name: &str) -> bool {
        self.collapsed.contains(name)
    }

    pub fn selected_device(&self) -> Option<BlockDevice> {
        self.flat_items()
            .get(self.list_state.selected().unwrap_or(0))
            .map(|item| item.device.clone())
    }

    pub fn selected_item(&self) -> Option<DeviceItem> {
        self.flat_items()
            .get(self.list_state.selected().unwrap_or(0))
            .cloned()
    }

    pub fn navigate_up(&mut self) {
        self.clear_force_confirm();
        self.details_scroll = 0;
        let index = self.list_state.selected().unwrap_or(0).saturating_sub(1);
        if !self.flat_items().is_empty() {
            self.list_state.select(Some(index));
        }
    }

    pub fn navigate_down(&mut self) {
        self.clear_force_confirm();
        self.details_scroll = 0;
        let len = self.flat_items().len();
        if len == 0 {
            return;
        }
        let index = (self.list_state.selected().unwrap_or(0) + 1).min(len - 1);
        self.list_state.select(Some(index));
    }

    pub fn select_first(&mut self) {
        self.clear_force_confirm();
        self.details_scroll = 0;
        if !self.flat_items().is_empty() {
            self.list_state.select(Some(0));
        }
    }

    pub fn select_last(&mut self) {
        self.clear_force_confirm();
        self.details_scroll = 0;
        let len = self.flat_items().len();
        if len > 0 {
            self.list_state.select(Some(len - 1));
        }
    }

    pub fn page_up(&mut self) {
        self.clear_force_confirm();
        self.details_scroll = 0;
        if self.flat_items().is_empty() {
            return;
        }
        let index = self
            .list_state
            .selected()
            .unwrap_or(0)
            .saturating_sub(PAGE_STEP);
        self.list_state.select(Some(index));
    }

    pub fn page_down(&mut self) {
        self.clear_force_confirm();
        self.details_scroll = 0;
        let len = self.flat_items().len();
        if len == 0 {
            return;
        }
        let index = (self.list_state.selected().unwrap_or(0) + PAGE_STEP).min(len - 1);
        self.list_state.select(Some(index));
    }

    /// Collapse / expand the selected node (any level that has children).
    pub fn toggle_collapse(&mut self) {
        self.clear_force_confirm();
        let Some(item) = self.selected_item() else {
            return;
        };
        if !Self::has_children(&item.device) {
            return;
        }
        if !self.collapsed.remove(&item.device.name) {
            self.collapsed.insert(item.device.name);
        }
        self.clamp_selection();
    }

    /// `/`: start a new search. A previous query is dropped so the input line
    /// always starts empty.
    pub fn begin_search(&mut self) {
        self.search.clear();
        self.search_editing = true;
        self.view_changed();
    }

    /// Append a typed character to the query (the list filters live).
    pub fn search_push_char(&mut self, c: char) {
        self.search.push(c);
        self.view_changed();
    }

    /// Backspace in the search input.
    pub fn search_backspace(&mut self) {
        if self.search.pop().is_some() {
            self.view_changed();
        }
    }

    /// Enter: leave the input line, keep the query applied.
    pub fn confirm_search(&mut self) {
        self.search_editing = false;
    }

    /// Esc in the input line: drop the query and leave the input line.
    pub fn cancel_search(&mut self) {
        self.search.clear();
        self.search_editing = false;
        self.view_changed();
    }

    /// Esc outside the input line: drop an applied query, return to the tree.
    pub fn clear_search(&mut self) {
        self.search.clear();
        self.view_changed();
    }

    /// `l`: show / hide loop devices, reporting the new state in the status bar.
    pub fn toggle_loops(&mut self) {
        self.hide_loops = !self.hide_loops;
        self.view_changed();
        self.action = ActionStatus::Success(
            if self.hide_loops {
                "Loop devices hidden"
            } else {
                "Loop devices shown"
            }
            .into(),
        );
    }

    /// `[`: scroll the details pane one row up.
    pub fn scroll_details_up(&mut self) {
        self.details_scroll = self.details_scroll.saturating_sub(1);
    }

    /// `]`: scroll the details pane one row down (bounded by
    /// [`DETAILS_SCROLL_MAX`]).
    pub fn scroll_details_down(&mut self) {
        self.details_scroll = self
            .details_scroll
            .saturating_add(1)
            .min(DETAILS_SCROLL_MAX);
    }

    /// The visible list changed (query / loop filter): keep the selection inside
    /// it and drop state that belongs to the previous view.
    fn view_changed(&mut self) {
        self.clamp_selection();
        self.clear_force_confirm();
        self.details_scroll = 0;
    }

    /// Find a device by name at any depth of the tree.
    pub fn find_device(&self, name: &str) -> Option<BlockDevice> {
        fn find(devices: &[BlockDevice], name: &str) -> Option<BlockDevice> {
            for device in devices {
                if device.name == name {
                    return Some(device.clone());
                }
                if let Some(children) = &device.children {
                    if let Some(found) = find(children, name) {
                        return Some(found);
                    }
                }
            }
            None
        }
        find(&self.devices, name)
    }

    /// First mount point currently reported for the device.
    fn current_mountpoint(&self, name: &str) -> Option<String> {
        self.find_device(name)
            .and_then(|device| device.mountpoints.first().cloned())
    }

    /// Enter: mount when unmounted, unmount when mounted, explain otherwise.
    pub fn enter_selected(&mut self, runner: &dyn CommandRunner) -> Option<InteractiveRequest> {
        self.clear_force_confirm();
        let Some(device) = self.selected_device() else {
            self.action = ActionStatus::Error("No device selected".into());
            return None;
        };

        if disk::is_mounted(&device) {
            self.unmount_selected(runner)
        } else if disk::is_mountable(&device) {
            self.mount_selected(runner)
        } else {
            self.action = ActionStatus::Error(format!(
                "{} cannot be mounted (type: {}, fstype: {})",
                device.name,
                device.device_type,
                device.fstype.as_deref().unwrap_or("none")
            ));
            None
        }
    }

    /// Mount the selected device with the configured backend.
    ///
    /// * udisksctl (default): `udisksctl mount -b <dev>` only. A failure is
    ///   reported as an error: no sudo escalation, no directory creation.
    /// * sudo (opt-in): the request describes `sudo mount -o … <dev> <target>`
    ///   where `<target>` lives under the configured root; `main` creates it
    ///   right before the terminal handoff.
    pub fn mount_selected(&mut self, runner: &dyn CommandRunner) -> Option<InteractiveRequest> {
        self.clear_force_confirm();
        let Some(device) = self.selected_device() else {
            self.action = ActionStatus::Error("No device selected".into());
            return None;
        };

        if !disk::is_mountable(&device) {
            self.action =
                ActionStatus::Error(format!("{} cannot be mounted (no filesystem)", device.name));
            return None;
        }

        if disk::is_mounted(&device) {
            self.action = ActionStatus::Error(format!(
                "{} is already mounted at {}",
                device.name,
                device.mountpoints.join(", ")
            ));
            return None;
        }

        self.action = ActionStatus::Mounting;

        match self.config.mount_mode {
            MountMode::Udisksctl => {
                self.mount_with_udisksctl(runner, &device);
                None
            }
            MountMode::Sudo => self.sudo_mount_request(device),
        }
    }

    /// Unprivileged mount: udisks2 picks the mount point itself.
    fn mount_with_udisksctl(&mut self, runner: &dyn CommandRunner, device: &BlockDevice) {
        let dev_path = disk::device_path(&device.name);

        match runner.output(&CommandSpec::new(
            "udisksctl",
            &["mount", "-b", dev_path.as_str()],
        )) {
            Ok(output) if output.success => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let _ = self.reload_devices(runner);
                let mount_point = self
                    .current_mountpoint(&device.name)
                    .or_else(|| parse_udisks_mountpoint(&stdout));
                self.action = ActionStatus::Success(format!(
                    "Mounted {} at {}",
                    dev_path,
                    mount_point.unwrap_or_else(|| "?".into())
                ));
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                self.action = ActionStatus::Error(mount_failed_message(&dev_path, &stderr));
            }
            Err(error) => {
                self.action = ActionStatus::Error(mount_failed_message(
                    &dev_path,
                    &format!("udisksctl error: {}", error),
                ));
            }
        }
    }

    /// Build the interactive `sudo mount` request.
    ///
    /// The mount point is *chosen* here (conflict-free under the configured
    /// root) but never created: that happens in `main` immediately before the
    /// handoff, so a cancelled or failed mount leaves no directory behind.
    fn sudo_mount_request(&mut self, device: BlockDevice) -> Option<InteractiveRequest> {
        let dev_path = disk::device_path(&device.name);
        let target = match mount::resolve_target(&self.config.mount_root, &device) {
            Ok(target) => target,
            Err(error) => {
                self.action = ActionStatus::Error(format!("Cannot mount {}: {}", dev_path, error));
                return None;
            }
        };
        let options = mount::mount_options(&device, mount::home_owner());
        Some(InteractiveRequest {
            program: "sudo".into(),
            args: vec![
                "mount".into(),
                "-o".into(),
                options,
                dev_path,
                target.display().to_string(),
            ],
            op: InteractiveOp::Mount,
            device_name: device.name,
            context: String::new(),
            fallback_args: None,
            mount_target: Some(target),
        })
    }

    /// Find processes using a device (for busy errors)
    fn find_busy_processes(runner: &dyn CommandRunner, dev_path: &str) -> String {
        // Try fuser first
        if let Ok(output) = runner.output(&CommandSpec::new("fuser", &["-vm", dev_path])) {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let info = format!("{}\n{}", stdout, stderr).trim().to_string();
            if !info.is_empty() && info != dev_path {
                return format!("Processes using {}:\n{}\n", dev_path, info);
            }
        }
        // Try lsof as fallback
        if let Ok(output) = runner.output(&CommandSpec::new("lsof", &[dev_path])) {
            let info = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !info.is_empty() {
                return format!("Processes using {}:\n{}\n", dev_path, info);
            }
        }
        String::new()
    }

    /// Shared pre-checks for unmount / force unmount. Returns `true` when the
    /// operation may proceed; otherwise the status bar already explains why and
    /// no command must be run.
    fn unmount_precheck(&mut self, device: &BlockDevice) -> bool {
        if !disk::is_mounted(device) {
            self.action = ActionStatus::Error(format!("{} is not mounted", device.name));
            return false;
        }

        if disk::is_system_mount(device) {
            self.action = ActionStatus::Error(format!(
                "Refusing to unmount {}: system mount",
                device.mountpoints.join(", ")
            ));
            return false;
        }

        // Multiple mount points (bind mounts, ...) can only be detached safely
        // by hand: refuse before any udisksctl / sudo attempt.
        if device.mountpoints.len() > 1 {
            self.action = ActionStatus::Error(format!(
                "{} has {} mount points; unmount manually (e.g. sudo umount <mp>)",
                device.name,
                device.mountpoints.len()
            ));
            return false;
        }

        true
    }

    /// Unmount the selected device with the configured backend.
    ///
    /// udisksctl (default): `udisksctl unmount -b <dev>` only, a failure is
    /// reported as an error (busy devices still list their processes).
    /// sudo (opt-in): an interactive `sudo umount <dev>` request.
    pub fn unmount_selected(&mut self, runner: &dyn CommandRunner) -> Option<InteractiveRequest> {
        self.clear_force_confirm();
        let Some(device) = self.selected_device() else {
            self.action = ActionStatus::Error("No device selected".into());
            return None;
        };

        if !self.unmount_precheck(&device) {
            return None;
        }

        self.action = ActionStatus::Unmounting;

        match self.config.mount_mode {
            MountMode::Udisksctl => {
                self.unmount_with_udisksctl(runner, &device);
                None
            }
            MountMode::Sudo => Some(InteractiveRequest {
                program: "sudo".into(),
                args: vec!["umount".into(), disk::device_path(&device.name)],
                op: InteractiveOp::Unmount,
                device_name: device.name,
                context: String::new(),
                fallback_args: None,
                mount_target: None,
            }),
        }
    }

    /// Unprivileged unmount: udisks2 detaches the mount point itself.
    fn unmount_with_udisksctl(&mut self, runner: &dyn CommandRunner, device: &BlockDevice) {
        let dev_path = disk::device_path(&device.name);

        match runner.output(&CommandSpec::new(
            "udisksctl",
            &["unmount", "-b", dev_path.as_str()],
        )) {
            Ok(output) if output.success => {
                let _ = self.reload_devices(runner);
                self.action = ActionStatus::Success(format!("Unmounted {}", dev_path));
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let context = self.describe_unmount_failure(runner, &dev_path, stderr);
                self.action = ActionStatus::Error(unmount_failed_message(&dev_path, &context));
            }
            Err(error) => {
                self.action = ActionStatus::Error(unmount_failed_message(
                    &dev_path,
                    &format!("udisksctl error: {}", error),
                ));
            }
        }
    }

    /// Add the busy diagnostics to a failed unmount, if the device is busy.
    fn describe_unmount_failure(
        &self,
        runner: &dyn CommandRunner,
        dev_path: &str,
        stderr: String,
    ) -> String {
        let mut context = stderr;
        if context.to_lowercase().contains("busy") {
            let busy_info = Self::find_busy_processes(runner, dev_path);
            if busy_info.is_empty() {
                context.push_str(" (device busy)");
            } else {
                context.push_str(&format!(" busy: {}", busy_info.trim()));
            }
        }
        context
    }

    /// Force unmount; the exact command depends on the configured mode.
    ///
    /// Two presses are always required: the first one runs the full pre-check
    /// and only arms the confirmation, the second one on the same device issues
    /// the command. Navigation and any other operation cancel the pending state.
    ///
    /// udisksctl mode runs `udisksctl unmount -b <dev> -f` through the runner
    /// (no terminal handoff); sudo mode returns the `sudo umount -l` request.
    pub fn force_unmount_selected(
        &mut self,
        runner: &dyn CommandRunner,
    ) -> Option<InteractiveRequest> {
        let Some(device) = self.selected_device() else {
            self.clear_force_confirm();
            self.action = ActionStatus::Error("No device selected".into());
            return None;
        };

        if !self.unmount_precheck(&device) {
            // A failed pre-check never arms (and drops an older arming).
            self.clear_force_confirm();
            return None;
        }

        if self.force_confirm.as_deref() != Some(device.name.as_str()) {
            self.force_confirm = Some(device.name.clone());
            self.action = ActionStatus::Error(force_confirm_prompt(&device.name));
            return None;
        }

        // Confirmed: consume the pending state and re-check, the device may
        // have changed state between the two presses.
        self.force_confirm = None;
        if !self.unmount_precheck(&device) {
            return None;
        }
        self.action = ActionStatus::Unmounting;

        match self.config.mount_mode {
            MountMode::Udisksctl => {
                self.force_unmount_with_udisksctl(runner, &device);
                None
            }
            MountMode::Sudo => {
                let dev_path = disk::device_path(&device.name);
                let mount_point = device.mountpoints.first().cloned().unwrap_or_default();
                let fallback_args = if mount_point.is_empty() {
                    None
                } else {
                    Some(vec!["umount".to_string(), "-l".to_string(), mount_point])
                };
                Some(InteractiveRequest {
                    program: "sudo".into(),
                    args: vec!["umount".into(), "-l".into(), dev_path],
                    op: InteractiveOp::ForceUnmount,
                    device_name: device.name,
                    context: String::new(),
                    fallback_args,
                    mount_target: None,
                })
            }
        }
    }

    /// Unprivileged forced unmount (`udisksctl unmount -f`).
    fn force_unmount_with_udisksctl(&mut self, runner: &dyn CommandRunner, device: &BlockDevice) {
        let dev_path = disk::device_path(&device.name);
        match runner.output(&CommandSpec::new(
            "udisksctl",
            &["unmount", "-b", dev_path.as_str(), "-f"],
        )) {
            Ok(output) if output.success => {
                let _ = self.reload_devices(runner);
                self.action = force_unmount_success(&dev_path);
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                self.action = ActionStatus::Error(force_unmount_failed(&dev_path, &stderr));
            }
            Err(error) => {
                self.action = ActionStatus::Error(force_unmount_failed(
                    &dev_path,
                    &format!("udisksctl error: {}", error),
                ));
            }
        }
    }

    /// Drop a pending force-unmount confirmation. When the status bar still
    /// shows the confirmation prompt it falls back to the neutral `Ready`
    /// state, so a cancelled prompt never lingers (it would otherwise be a
    /// stale red warning).
    pub fn clear_force_confirm(&mut self) {
        if self.force_confirm.take().is_none() {
            return;
        }
        let shows_prompt = match &self.action {
            ActionStatus::Success(message) | ActionStatus::Error(message) => {
                message.starts_with(FORCE_CONFIRM_PROMPT)
            }
            _ => false,
        };
        if shows_prompt {
            self.action = ActionStatus::None;
        }
    }

    /// Called by `main` after the interactive command finished: refresh first,
    /// then report the real mount point (falling back to the planned one).
    pub fn finish(&mut self, runner: &dyn CommandRunner, request: InteractiveRequest, ok: bool) {
        let _ = self.reload_devices(runner);
        let mount_point = if request.op == InteractiveOp::Mount && ok {
            self.current_mountpoint(&request.device_name)
                .or_else(|| request.args.last().cloned())
        } else {
            None
        };
        self.action = outcome_status(&request, ok, mount_point);
    }

    /// Manual refresh ('r'): reload devices, report the result in the status bar.
    /// The success message replaces any stale status (e.g. an old error).
    pub fn refresh(&mut self, runner: &dyn CommandRunner) {
        self.clear_force_confirm();
        match self.reload_devices(runner) {
            Ok(()) => self.action = ActionStatus::Success("Refreshed".into()),
            Err(e) => self.action = ActionStatus::Error(format!("Refresh failed: {}", e)),
        }
    }

    /// Replace the device tree only when it actually changed.
    ///
    /// Returns `true` when the tree was replaced (the caller must redraw),
    /// `false` when it was identical. The selection and collapse state survive
    /// via [`App::set_devices`].
    pub fn update_devices(&mut self, devices: Vec<BlockDevice>) -> bool {
        if devices == self.devices {
            return false;
        }
        self.set_devices(devices);
        true
    }

    /// Periodic refresh used by the main loop. Failures stay silent so a
    /// transient `lsblk` error never overwrites the status bar.
    pub fn auto_refresh(&mut self, runner: &dyn CommandRunner) -> bool {
        match disk::get_block_devices(runner) {
            Ok(devices) => self.update_devices(devices),
            Err(_) => false,
        }
    }

    fn reload_devices(&mut self, runner: &dyn CommandRunner) -> anyhow::Result<()> {
        let devices = disk::get_block_devices(runner)?;
        self.set_devices(devices);
        Ok(())
    }

    /// Replace the device tree while keeping the selection on the same device.
    ///
    /// A pending force-unmount confirmation only applies to the device it was
    /// armed on: when a refresh makes the selection point at a different device
    /// (usually because the armed one disappeared), the confirmation is dropped
    /// and the prompt falls back to `Ready`.
    pub fn set_devices(&mut self, devices: Vec<BlockDevice>) {
        let previous = self.selected_device().map(|device| device.name);
        self.devices = devices;
        self.restore_selection(previous.clone());
        // The details pane only scrolls when the selection moves to another
        // device: a refresh that keeps the selection keeps the scroll offset.
        if self.selected_device().map(|device| device.name) != previous {
            self.details_scroll = 0;
        }
        self.drop_orphaned_force_confirm();
    }

    /// Drop a pending force confirmation that no longer belongs to the selected
    /// device.
    fn drop_orphaned_force_confirm(&mut self) {
        let Some(pending) = self.force_confirm.clone() else {
            return;
        };
        let selected = self.selected_device().map(|device| device.name);
        if selected.as_deref() != Some(pending.as_str()) {
            self.clear_force_confirm();
        }
    }

    fn restore_selection(&mut self, previous_name: Option<String>) {
        let items = self.flat_items();
        if items.is_empty() {
            self.list_state.select(None);
            return;
        }
        let fallback = self.list_state.selected().unwrap_or(0);
        let index = previous_name
            .and_then(|name| items.iter().position(|item| item.device.name == name))
            .unwrap_or(fallback);
        self.list_state.select(Some(index.min(items.len() - 1)));
    }

    fn clamp_selection(&mut self) {
        let len = self.flat_items().len();
        if len == 0 {
            self.list_state.select(None);
            return;
        }
        let index = self.list_state.selected().unwrap_or(0).min(len - 1);
        self.list_state.select(Some(index));
    }
}

/// `true` when `lsblk` reports the device as a loop device.
fn is_loop(device: &BlockDevice) -> bool {
    device.device_type.eq_ignore_ascii_case(LOOP_DEVICE_TYPE)
}

/// `true` when any searchable field of `device` contains `query`.
///
/// `query` is already lowercased (see [`App::search_query`]); name, label,
/// model, fstype and every mount point are compared case-insensitively. Vendor,
/// serial and UUID are deliberately not searchable.
fn device_matches_query(device: &BlockDevice, query: &str) -> bool {
    let fields = [
        Some(device.name.as_str()),
        device.label.as_deref(),
        device.model.as_deref(),
        device.fstype.as_deref(),
    ];
    if fields
        .iter()
        .flatten()
        .any(|field| field.to_lowercase().contains(query))
    {
        return true;
    }
    device
        .mountpoints
        .iter()
        .any(|mountpoint| mountpoint.to_lowercase().contains(query))
}

/// Status text of the first `f` press, before the force unmount is issued.
fn force_confirm_prompt(device_name: &str) -> String {
    format!(
        "{} {} (lazy unmount may lose data)",
        FORCE_CONFIRM_PROMPT, device_name
    )
}

/// Extract the mount point from `udisksctl mount` output:
/// `Mounted /dev/sdb1 at /media/user/USB.`
///
/// The split happens on the *first* ` at `: the device part never contains one,
/// while the mount point may (`/media/user/My at Disk`). The sentence period is
/// only removed when the dot-terminated path does not exist, so a directory
/// whose name really ends in `.` keeps its name.
fn parse_udisks_mountpoint(stdout: &str) -> Option<String> {
    let line = stdout.trim();
    let (_, mount_point) = line.split_once(" at ")?;
    let mount_point = mount_point.trim();
    let mount_point = strip_sentence_period(mount_point);
    if mount_point.is_empty() {
        None
    } else {
        Some(mount_point.to_string())
    }
}

/// Drop the trailing `.` udisksctl appends, unless it is part of a real path.
fn strip_sentence_period(candidate: &str) -> &str {
    match candidate.strip_suffix('.') {
        Some(stripped) if !stripped.is_empty() && !Path::new(candidate).exists() => stripped,
        _ => candidate,
    }
}

/// Status text for a failed `mount`.
fn mount_failed_message(dev_path: &str, context: &str) -> String {
    format!("Mount failed for {}: {}", dev_path, context)
}

/// Status text for a failed `unmount`, with the force hint when busy.
fn unmount_failed_message(dev_path: &str, context: &str) -> String {
    let hint = if context.to_lowercase().contains("busy") {
        "\nDevice busy. Press 'f' for force lazy unmount."
    } else {
        ""
    };
    format!("Unmount failed for {}: {}{}", dev_path, context, hint)
}

fn force_unmount_success(dev_path: &str) -> ActionStatus {
    ActionStatus::Success(format!(
        "Force unmounted {} (lazy: detached, cleaned up when no longer busy)",
        dev_path
    ))
}

fn force_unmount_failed(dev_path: &str, context: &str) -> String {
    format!("Force unmount failed for {}: {}", dev_path, context)
}

/// Status bar text for a finished interactive operation.
fn outcome_status(
    request: &InteractiveRequest,
    ok: bool,
    mount_point: Option<String>,
) -> ActionStatus {
    let dev_path = disk::device_path(&request.device_name);
    match (request.op, ok) {
        (InteractiveOp::Mount, true) => ActionStatus::Success(format!(
            "Mounted {} at {}",
            dev_path,
            mount_point.unwrap_or_else(|| "?".into())
        )),
        (InteractiveOp::Mount, false) => {
            ActionStatus::Error(mount_failed_message(&dev_path, &request.context))
        }
        (InteractiveOp::Unmount, true) => ActionStatus::Success(format!("Unmounted {}", dev_path)),
        (InteractiveOp::Unmount, false) => {
            ActionStatus::Error(unmount_failed_message(&dev_path, &request.context))
        }
        (InteractiveOp::ForceUnmount, true) => force_unmount_success(&dev_path),
        (InteractiveOp::ForceUnmount, false) => {
            ActionStatus::Error(force_unmount_failed(&dev_path, &request.context))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::FakeRunner;

    fn bd(name: &str, device_type: &str, fstype: Option<&str>, mp: Option<&str>) -> BlockDevice {
        BlockDevice {
            name: name.to_string(),
            size: "1G".to_string(),
            device_type: device_type.to_string(),
            fstype: fstype.map(str::to_string),
            mountpoints: mp.map(str::to_string).into_iter().collect(),
            label: None,
            ro: false,
            rm: false,
            model: None,
            vendor: None,
            tran: None,
            serial: None,
            uuid: None,
            children: None,
        }
    }

    fn with_mounts(mut device: BlockDevice, mountpoints: &[&str]) -> BlockDevice {
        device.mountpoints = mountpoints.iter().map(|mp| mp.to_string()).collect();
        device
    }

    fn with_children(mut device: BlockDevice, children: Vec<BlockDevice>) -> BlockDevice {
        device.children = Some(children);
        device
    }

    /// usb disk → two partitions, one of which carries a crypt child
    fn sample_tree() -> Vec<BlockDevice> {
        vec![
            with_children(
                bd("sda", "disk", None, None),
                vec![
                    bd("sda1", "part", Some("ext4"), Some("/boot")),
                    bd("sda2", "part", Some("swap"), None),
                ],
            ),
            with_children(
                bd("sdb", "disk", None, None),
                vec![with_children(
                    bd("sdb1", "part", Some("crypto_LUKS"), None),
                    vec![bd("sdb1-crypt", "crypt", Some("ext4"), None)],
                )],
            ),
        ]
    }

    fn app_with(devices: Vec<BlockDevice>) -> App {
        App::new(devices)
    }

    /// A runner with no queued results: any command would panic, so a test
    /// asserting zero calls also proves that no command was attempted.
    fn no_commands() -> FakeRunner {
        FakeRunner::new()
    }

    /// Scratch directory for sudo-mode tests; removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "mount-tui-app-{}-{}-{}",
                tag,
                std::process::id(),
                unique
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Sudo-mode app whose root is a scratch directory (never the real HOME).
    fn sudo_app(devices: Vec<BlockDevice>, root: &Path) -> App {
        App::with_config(
            devices,
            Config {
                mount_mode: MountMode::Sudo,
                mount_root: root.to_path_buf(),
            },
        )
    }

    /// Mount options expected for a filesystem with a single-owner mapping when
    /// the owner is the current user's `$HOME` directory.
    fn expected_owner_options(home: &Path) -> String {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(home).unwrap();
        format!(
            "nosuid,nodev,uid={},gid={},umask=0022",
            metadata.uid(),
            metadata.gid()
        )
    }

    #[test]
    fn flat_items_expands_every_level() {
        let app = app_with(sample_tree());
        let names: Vec<(String, usize)> = app
            .flat_items()
            .into_iter()
            .map(|item| (item.device.name, item.depth))
            .collect();
        assert_eq!(
            names,
            vec![
                ("sda".to_string(), 0),
                ("sda1".to_string(), 1),
                ("sda2".to_string(), 1),
                ("sdb".to_string(), 0),
                ("sdb1".to_string(), 1),
                ("sdb1-crypt".to_string(), 2),
            ]
        );
    }

    #[test]
    fn collapse_hides_all_descendants() {
        let mut app = app_with(sample_tree());
        app.list_state.select(Some(3)); // sdb
        assert_eq!(app.selected_device().unwrap().name, "sdb");
        app.toggle_collapse();
        assert!(app.is_collapsed("sdb"));
        let names: Vec<String> = app
            .flat_items()
            .into_iter()
            .map(|item| item.device.name)
            .collect();
        assert_eq!(names, vec!["sda", "sda1", "sda2", "sdb"]);

        // Toggling again expands.
        app.toggle_collapse();
        assert!(!app.is_collapsed("sdb"));
        assert_eq!(app.flat_items().len(), 6);
    }

    #[test]
    fn nested_node_can_be_collapsed() {
        let mut app = app_with(sample_tree());
        app.list_state.select(Some(4)); // sdb1 (depth 1, has a child)
        app.toggle_collapse();
        assert!(app.is_collapsed("sdb1"));
        let names: Vec<String> = app
            .flat_items()
            .into_iter()
            .map(|item| item.device.name)
            .collect();
        assert_eq!(names, vec!["sda", "sda1", "sda2", "sdb", "sdb1"]);
        // A leaf cannot be collapsed.
        app.list_state.select(Some(1)); // sda1
        app.toggle_collapse();
        assert_eq!(app.collapsed.len(), 1);
        assert!(app.is_collapsed("sdb1"));
    }

    #[test]
    fn collapse_state_survives_refresh_by_name() {
        let mut app = app_with(sample_tree());
        app.collapsed.insert("sdb".to_string());
        // Re-parse the same tree, but with an extra disk in front: indices shift.
        let mut devices = vec![bd("sdc", "disk", None, None)];
        devices.extend(sample_tree());
        app.set_devices(devices);
        // sdb is now at a different index but stays collapsed.
        assert!(app.is_collapsed("sdb"));
        let names: Vec<String> = app
            .flat_items()
            .into_iter()
            .map(|item| item.device.name)
            .collect();
        assert!(names.contains(&"sdb".to_string()));
        assert!(!names.contains(&"sdb1".to_string()));
        assert!(!names.contains(&"sdb1-crypt".to_string()));
    }

    #[test]
    fn selection_follows_device_name_across_refresh() {
        let mut app = app_with(sample_tree());
        app.list_state.select(Some(4)); // sdb1
        let mut devices = vec![bd("sdc", "disk", None, None)];
        devices.extend(sample_tree());
        app.set_devices(devices);
        assert_eq!(app.selected_device().unwrap().name, "sdb1");
        assert_eq!(app.list_state.selected(), Some(5));
    }

    #[test]
    fn selection_is_clamped_when_device_disappears() {
        let mut app = app_with(sample_tree());
        app.list_state.select(Some(5)); // sdb1-crypt
        app.set_devices(vec![bd("sda", "disk", None, None)]);
        assert_eq!(app.list_state.selected(), Some(0));
        assert_eq!(app.selected_device().unwrap().name, "sda");

        app.set_devices(Vec::new());
        assert_eq!(app.list_state.selected(), None);
        assert!(app.selected_device().is_none());
    }

    #[test]
    fn navigation_stays_in_range() {
        let mut app = app_with(sample_tree());
        app.select_first();
        app.navigate_up();
        assert_eq!(app.list_state.selected(), Some(0));
        app.select_last();
        app.navigate_down();
        assert_eq!(app.list_state.selected(), Some(5));
        app.page_up();
        assert_eq!(app.list_state.selected(), Some(0));
        app.page_down();
        assert_eq!(app.list_state.selected(), Some(5));
        app.navigate_up();
        assert_eq!(app.list_state.selected(), Some(4));
    }

    #[test]
    fn unmounting_a_system_mount_is_refused() {
        let mut app = app_with(vec![bd("sda1", "part", Some("ext4"), Some("/"))]);
        app.list_state.select(Some(0));
        let runner = no_commands();
        let request = app.unmount_selected(&runner);
        assert!(request.is_none());
        assert_eq!(runner.call_count(), 0);
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(message.contains("Refusing to unmount /"), "{}", message);
                assert!(message.contains("system mount"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn force_unmounting_a_system_mount_is_refused() {
        let mut app = app_with(vec![bd("sda2", "part", Some("ext4"), Some("/usr"))]);
        app.list_state.select(Some(0));
        let runner = no_commands();
        let request = app.force_unmount_selected(&runner);
        assert!(request.is_none());
        assert!(matches!(app.action, ActionStatus::Error(_)));
        assert_eq!(runner.call_count(), 0);
    }

    #[test]
    fn update_devices_ignores_an_identical_tree() {
        let mut app = app_with(sample_tree());
        assert!(!app.update_devices(sample_tree()));
    }

    #[test]
    fn update_devices_replaces_a_changed_tree_and_keeps_selection() {
        let mut app = app_with(sample_tree());
        app.collapsed.insert("sda".to_string());
        app.list_state.select(Some(2)); // sdb1 (sda's children are hidden)
        assert_eq!(app.selected_device().unwrap().name, "sdb1");

        let mut devices = vec![bd("sdc", "disk", None, None)];
        devices.extend(sample_tree());

        assert!(app.update_devices(devices));
        assert_eq!(app.selected_device().unwrap().name, "sdb1");
        assert!(app.is_collapsed("sda"));
    }

    #[test]
    fn update_devices_detects_flag_and_mountpoint_changes() {
        let mut app = app_with(vec![bd("sdb1", "part", Some("ext4"), None)]);
        let mut changed = bd("sdb1", "part", Some("ext4"), Some("/mnt/a"));
        assert!(app.update_devices(vec![changed.clone()]));
        // Equal again: no redraw needed.
        assert!(!app.update_devices(vec![changed.clone()]));

        changed.ro = true;
        assert!(app.update_devices(vec![changed]));
    }

    #[test]
    fn unmounting_a_multi_mount_device_is_refused_without_commands() {
        let mut app = app_with(vec![with_mounts(
            bd("sdb1", "part", Some("ext4"), None),
            &["/mnt/a", "/mnt/b"],
        )]);
        app.list_state.select(Some(0));

        let runner = no_commands();
        assert!(app.unmount_selected(&runner).is_none());
        assert_eq!(runner.call_count(), 0);
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(message.contains("has 2 mount points"), "{}", message);
                assert!(message.contains("unmount manually"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn force_unmounting_a_multi_mount_device_is_refused() {
        let mut app = app_with(vec![with_mounts(
            bd("sdb1", "part", Some("ext4"), None),
            &["/mnt/a", "/mnt/b"],
        )]);
        app.list_state.select(Some(0));

        assert!(app.force_unmount_selected(&no_commands()).is_none());
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(message.contains("has 2 mount points"), "{}", message);
                assert!(message.contains("unmount manually"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    /// A mounted, non-system device: the force pre-check passes.
    fn forceable_app() -> App {
        let mut app = app_with(vec![with_mounts(
            bd("sdb1", "part", Some("ext4"), None),
            &["/mnt/data"],
        )]);
        app.list_state.select(Some(0));
        app
    }

    /// The same device in sudo mode: the second `f` produces a request.
    fn sudo_forceable_app(root: &Path) -> App {
        let mut app = sudo_app(
            vec![with_mounts(
                bd("sdb1", "part", Some("ext4"), None),
                &["/mnt/data"],
            )],
            root,
        );
        app.list_state.select(Some(0));
        app
    }

    fn force_prompt_shown(app: &App) -> bool {
        match &app.action {
            ActionStatus::Success(message) | ActionStatus::Error(message) => {
                message.starts_with(FORCE_CONFIRM_PROMPT)
            }
            _ => false,
        }
    }

    #[test]
    fn force_unmount_arms_on_the_first_press_and_runs_on_the_second() {
        let root = TempDir::new("force-sudo");
        let mut app = sudo_forceable_app(root.path());
        let runner = no_commands();

        // First `f`: pre-check passes, the confirmation is armed, nothing runs.
        assert!(app.force_unmount_selected(&runner).is_none());
        assert_eq!(app.force_confirm.as_deref(), Some("sdb1"));
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(
                    message.contains("Press f again to force-unmount sdb1"),
                    "{}",
                    message
                );
                assert!(
                    message.contains("lazy unmount may lose data"),
                    "{}",
                    message
                );
            }
            other => panic!("expected the confirmation prompt, got {:?}", other),
        }
        assert_eq!(runner.call_count(), 0, "arming must not run anything");

        // Second `f` on the same device issues the real request.
        let request = app
            .force_unmount_selected(&runner)
            .expect("second press issues the request");
        assert_eq!(request.op, InteractiveOp::ForceUnmount);
        assert_eq!(
            request.args,
            vec!["umount".to_string(), "-l".into(), "/dev/sdb1".into()]
        );
        assert_eq!(
            request.fallback_args,
            Some(vec!["umount".to_string(), "-l".into(), "/mnt/data".into()])
        );
        assert_eq!(request.mount_target, None);
        assert_eq!(app.force_confirm, None);
        assert_eq!(runner.call_count(), 0, "sudo mode never touches the runner");
    }

    #[test]
    fn udisksctl_force_runs_the_forced_unmount_through_the_runner() {
        let runner = FakeRunner::new()
            .push_ok("Unmounted /dev/sdb1.")
            .push_ok(MOUNTED_JSON);
        let mut app = forceable_app();

        assert!(
            app.force_unmount_selected(&runner).is_none(),
            "first press arms"
        );
        assert!(
            app.force_unmount_selected(&runner).is_none(),
            "second press runs"
        );
        assert_eq!(
            runner.calls()[0],
            CommandSpec::new("udisksctl", &["unmount", "-b", "/dev/sdb1", "-f"]),
            "udisksctl mode uses the non-interactive forced unmount"
        );
        assert_eq!(runner.call_count(), 2, "the tree is reloaded afterwards");
        assert_eq!(app.force_confirm, None);
        assert!(
            matches!(&app.action, ActionStatus::Success(message) if message.contains("Force unmounted /dev/sdb1")),
            "{:?}",
            app.action
        );
    }

    #[test]
    fn udisksctl_force_failure_is_reported_without_sudo() {
        let runner = FakeRunner::new().push_fail("Error: device is busy");
        let mut app = forceable_app();

        assert!(app.force_unmount_selected(&runner).is_none());
        assert!(app.force_unmount_selected(&runner).is_none());
        assert_eq!(runner.call_count(), 1, "no retry, no sudo fallback");
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(
                    message.contains("Force unmount failed for /dev/sdb1"),
                    "{}",
                    message
                );
                assert!(message.contains("busy"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn list_navigation_cancels_the_force_confirmation_prompt() {
        let navigations: [fn(&mut App); 7] = [
            App::navigate_up,
            App::navigate_down,
            App::select_first,
            App::select_last,
            App::page_up,
            App::page_down,
            App::toggle_collapse,
        ];

        for navigate in navigations {
            let mut app = forceable_app();
            app.force_unmount_selected(&no_commands());
            assert!(app.force_confirm.is_some());
            assert!(force_prompt_shown(&app));

            navigate(&mut app);

            assert_eq!(
                app.force_confirm, None,
                "pending confirmation must be dropped"
            );
            assert_eq!(
                app.action,
                ActionStatus::None,
                "a cancelled confirmation must not linger in the status bar"
            );
        }
    }

    #[test]
    fn other_operations_cancel_the_force_confirmation() {
        // `u` on the armed device.
        let mut app = forceable_app();
        app.force_unmount_selected(&no_commands());
        let runner = FakeRunner::new()
            .push_ok("Unmounted /dev/sdb1.")
            .push_ok(UNMOUNTED_JSON);
        assert!(app.unmount_selected(&runner).is_none());
        assert_eq!(app.force_confirm, None);
        assert!(!force_prompt_shown(&app));

        // `m` on the armed (already mounted) device is refused, but still cancels.
        let mut app = forceable_app();
        app.force_unmount_selected(&no_commands());
        let runner = no_commands();
        assert!(app.mount_selected(&runner).is_none());
        assert_eq!(app.force_confirm, None);
        assert!(!force_prompt_shown(&app));

        // Enter follows the mounted branch (unmount) and also cancels.
        let mut app = forceable_app();
        app.force_unmount_selected(&no_commands());
        let runner = FakeRunner::new()
            .push_ok("Unmounted /dev/sdb1.")
            .push_ok(UNMOUNTED_JSON);
        assert!(app.enter_selected(&runner).is_none());
        assert_eq!(app.force_confirm, None);

        // Manual refresh cancels; its own status then replaces the prompt.
        let mut app = forceable_app();
        app.force_unmount_selected(&no_commands());
        let runner = FakeRunner::new().push_ok(MOUNTED_JSON);
        app.refresh(&runner);
        assert_eq!(app.force_confirm, None);
        assert_eq!(app.action, ActionStatus::Success("Refreshed".into()));
    }

    #[test]
    fn force_precheck_failure_does_not_arm_the_confirmation() {
        // Not mounted.
        let mut app = app_with(vec![bd("sdb1", "part", Some("ext4"), None)]);
        app.list_state.select(Some(0));
        assert!(app.force_unmount_selected(&no_commands()).is_none());
        assert_eq!(app.force_confirm, None);
        assert!(!force_prompt_shown(&app));
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(message.contains("is not mounted"), "{}", message)
            }
            other => panic!("expected error, got {:?}", other),
        }

        // System mount.
        let mut app = app_with(vec![bd("sda2", "part", Some("ext4"), Some("/usr"))]);
        app.list_state.select(Some(0));
        assert!(app.force_unmount_selected(&no_commands()).is_none());
        assert_eq!(app.force_confirm, None);
        assert!(!force_prompt_shown(&app));

        // Multiple mount points.
        let mut app = app_with(vec![with_mounts(
            bd("sdb1", "part", Some("ext4"), None),
            &["/mnt/a", "/mnt/b"],
        )]);
        app.list_state.select(Some(0));
        assert!(app.force_unmount_selected(&no_commands()).is_none());
        assert_eq!(app.force_confirm, None);
        assert!(!force_prompt_shown(&app));
    }

    #[test]
    fn second_force_press_rechecks_before_running() {
        let mut app = forceable_app();
        app.force_unmount_selected(&no_commands());
        assert!(app.force_confirm.is_some());

        // The device got unmounted between the two presses: the second press
        // must fail the pre-check instead of running `umount`.
        app.set_devices(vec![bd("sdb1", "part", Some("ext4"), None)]);
        assert!(app.force_unmount_selected(&no_commands()).is_none());
        assert_eq!(app.force_confirm, None);
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(message.contains("is not mounted"), "{}", message)
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn a_refresh_that_keeps_the_armed_device_keeps_the_prompt() {
        let mut app = forceable_app();
        app.force_unmount_selected(&no_commands());
        assert_eq!(app.force_confirm.as_deref(), Some("sdb1"));

        // Auto-refresh replaced the tree (same device, different details).
        let mut refreshed = with_mounts(bd("sdb1", "part", Some("vfat"), None), &["/mnt/data"]);
        refreshed.label = Some("USBKEY".to_string());
        app.set_devices(vec![refreshed]);

        assert_eq!(app.force_confirm.as_deref(), Some("sdb1"));
        assert!(force_prompt_shown(&app));
    }

    #[test]
    fn a_refresh_that_drops_the_armed_device_clears_the_prompt() {
        let mut app = forceable_app();
        app.force_unmount_selected(&no_commands());
        assert_eq!(app.force_confirm.as_deref(), Some("sdb1"));
        assert!(force_prompt_shown(&app));

        // The armed device disappeared (unplugged between two refreshes): the
        // pending confirmation no longer applies to the selected device, so it
        // is dropped and the status bar falls back to `Ready`.
        app.set_devices(vec![bd("sda", "disk", None, None)]);
        assert_eq!(app.force_confirm, None);
        assert_eq!(
            app.action,
            ActionStatus::None,
            "a stale prompt must not linger in the status bar"
        );
        assert_eq!(app.selected_device().unwrap().name, "sda");
    }

    #[test]
    fn an_empty_tree_after_a_refresh_clears_the_prompt() {
        let mut app = forceable_app();
        app.force_unmount_selected(&no_commands());
        app.set_devices(Vec::new());
        assert_eq!(app.force_confirm, None);
        assert!(app.selected_device().is_none());
        assert_eq!(app.action, ActionStatus::None);
    }

    #[test]
    fn sudo_mount_uses_the_configured_root_and_owner_options() {
        with_temp_home(|home| {
            let root = TempDir::new("sudo-root");
            let runner = no_commands();
            let mut app = sudo_app(vec![bd("sdb1", "part", Some("vfat"), None)], root.path());
            app.list_state.select(Some(0));

            let request = app.mount_selected(&runner).expect("sudo mount request");
            assert_eq!(request.program, "sudo");
            assert_eq!(request.op, InteractiveOp::Mount);
            assert_eq!(
                request.args,
                vec![
                    "mount".to_string(),
                    "-o".into(),
                    expected_owner_options(home),
                    "/dev/sdb1".into(),
                    root.path().join("sdb1").display().to_string(),
                ]
            );
            assert_eq!(
                request.mount_target,
                Some(root.path().join("sdb1")),
                "main is told which directory to create"
            );
            assert!(request.context.is_empty(), "{}", request.context);
            // sudo mode never consults udisksctl first.
            assert_eq!(runner.call_count(), 0);
            // The App only describes the target, it must never create it.
            assert!(
                !root.path().join("sdb1").exists(),
                "building the request must not create the mount point"
            );
        });
    }

    #[test]
    fn sudo_mount_prefers_the_label_and_reuses_a_free_directory() {
        let root = TempDir::new("sudo-label");
        let runner = no_commands();
        let mut device = bd("sdb1", "part", Some("ext4"), None);
        device.label = Some("  My/Stick  ".to_string());
        let mut app = sudo_app(vec![device], root.path());
        app.list_state.select(Some(0));

        let request = app.mount_selected(&runner).expect("sudo mount request");
        assert_eq!(
            request.args.last().unwrap(),
            &root.path().join("My_Stick").display().to_string(),
            "a sanitised label names the target"
        );
        assert_eq!(request.mount_target, Some(root.path().join("My_Stick")));
        // ext4 keeps the base options only.
        assert_eq!(request.args[2], mount::BASE_MOUNT_OPTIONS);
    }

    #[test]
    fn sudo_mount_refuses_a_symlinked_target() {
        let root = TempDir::new("sudo-symlink");
        let real = root.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, root.path().join("sdb1")).unwrap();

        let runner = no_commands();
        let mut app = sudo_app(vec![bd("sdb1", "part", Some("ext4"), None)], root.path());
        app.list_state.select(Some(0));

        assert!(app.mount_selected(&runner).is_none());
        assert_eq!(runner.call_count(), 0);
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(message.contains("Cannot mount /dev/sdb1"), "{}", message);
                assert!(message.contains("symbolic link"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn udisksctl_mount_failure_reports_an_error_without_sudo_or_directories() {
        with_temp_home(|home| {
            let runner = FakeRunner::new().push_fail("Error: not authorized to mount /dev/sdb1");
            let mut app = app_with(vec![bd("sdb1", "part", Some("vfat"), None)]);
            app.list_state.select(Some(0));

            assert!(
                app.mount_selected(&runner).is_none(),
                "the default mode produces no interactive request"
            );
            assert_eq!(runner.call_count(), 1, "only udisksctl ran");
            match &app.action {
                ActionStatus::Error(message) => {
                    assert!(
                        message.contains("Mount failed for /dev/sdb1"),
                        "{}",
                        message
                    );
                    assert!(message.contains("not authorized"), "{}", message);
                }
                other => panic!("expected error, got {:?}", other),
            }
            assert!(
                !home.join("mnt").exists(),
                "the default mode must not create a mount point"
            );
        });
    }

    #[test]
    fn udisksctl_mount_spawn_error_is_reported() {
        let runner = FakeRunner::new().push_err("udisksctl not found");
        let mut app = app_with(vec![bd("sdb1", "part", Some("vfat"), None)]);
        app.list_state.select(Some(0));

        assert!(app.mount_selected(&runner).is_none());
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(
                    message.contains(
                        "Mount failed for /dev/sdb1: udisksctl error: udisksctl not found"
                    ),
                    "{}",
                    message
                );
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn multi_mount_system_protection_checks_every_mountpoint() {
        let mut app = app_with(vec![with_mounts(
            bd("sda1", "part", Some("ext4"), None),
            &["/mnt/data", "/"],
        )]);
        app.list_state.select(Some(0));

        let runner = no_commands();
        assert!(app.unmount_selected(&runner).is_none());
        assert_eq!(runner.call_count(), 0);
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(message.contains("system mount"), "{}", message);
                assert!(!message.contains("mount points;"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn already_mounted_error_lists_all_mountpoints() {
        let mut app = app_with(vec![with_mounts(
            bd("sdb1", "part", Some("ext4"), None),
            &["/mnt/a", "/mnt/b"],
        )]);
        app.list_state.select(Some(0));

        let runner = no_commands();
        assert!(app.mount_selected(&runner).is_none());
        assert_eq!(runner.call_count(), 0);
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(message.contains("/mnt/a"), "{}", message);
                assert!(message.contains("/mnt/b"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn unmounting_a_protected_subtree_is_refused() {
        let mut app = app_with(vec![bd("sda1", "part", Some("ext4"), Some("/var/lib"))]);
        app.list_state.select(Some(0));
        let runner = no_commands();
        assert!(app.unmount_selected(&runner).is_none());
        assert_eq!(runner.call_count(), 0);
        assert!(matches!(app.action, ActionStatus::Error(_)));
    }

    #[test]
    fn enter_on_non_mountable_device_explains_why() {
        let mut app = app_with(vec![bd("sdb1", "part", Some("LVM2_member"), None)]);
        app.list_state.select(Some(0));
        let runner = no_commands();
        assert!(app.enter_selected(&runner).is_none());
        assert_eq!(runner.call_count(), 0);
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(message.contains("sdb1"), "{}", message);
                assert!(message.contains("LVM2_member"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn mount_selected_rejects_non_filesystem() {
        let mut app = app_with(vec![bd("sdb1", "part", Some("swap"), None)]);
        app.list_state.select(Some(0));
        let runner = no_commands();
        assert!(app.mount_selected(&runner).is_none());
        assert_eq!(runner.call_count(), 0);
        assert!(matches!(app.action, ActionStatus::Error(_)));
    }

    #[test]
    fn parses_udisksctl_mount_output() {
        assert_eq!(
            parse_udisks_mountpoint("Mounted /dev/sdb1 at /media/user/USB."),
            Some("/media/user/USB".to_string())
        );
        assert_eq!(
            parse_udisks_mountpoint("Mounted /dev/sdb1 at /media/user/My Disk"),
            Some("/media/user/My Disk".to_string())
        );
        assert_eq!(parse_udisks_mountpoint("something else"), None);
    }

    #[test]
    fn a_mountpoint_containing_the_separator_is_parsed_whole() {
        assert_eq!(
            parse_udisks_mountpoint("Mounted /dev/sdb1 at /media/user/My at Disk."),
            Some("/media/user/My at Disk".to_string())
        );
        assert_eq!(
            parse_udisks_mountpoint("Mounted /dev/sdb1 at /media/user/a at b at c"),
            Some("/media/user/a at b at c".to_string())
        );
    }

    #[test]
    fn a_mountpoint_ending_in_a_dot_keeps_it_when_the_path_exists() {
        let root = TempDir::new("dot");
        let dir = root.path().join("USB.");
        std::fs::create_dir_all(&dir).unwrap();

        // Without the sentence period the dot is part of the real path.
        assert_eq!(
            parse_udisks_mountpoint(&format!("Mounted /dev/sdb1 at {}", dir.display())),
            Some(dir.display().to_string())
        );
        // With the period the candidate does not exist, so only that one is
        // dropped and the real directory is reported.
        assert_eq!(
            parse_udisks_mountpoint(&format!("Mounted /dev/sdb1 at {}.", dir.display())),
            Some(dir.display().to_string())
        );
    }

    #[test]
    fn finish_status_reports_real_mountpoint() {
        let request = InteractiveRequest {
            program: "sudo".into(),
            args: vec![
                "mount".into(),
                "/dev/sdb1".into(),
                "/home/u/mnt/sdb1".into(),
            ],
            op: InteractiveOp::Mount,
            device_name: "sdb1".into(),
            context: "udisksctl: not authorized".into(),
            fallback_args: None,
            mount_target: None,
        };
        assert_eq!(
            outcome_status(&request, true, Some("/media/u/USB".into())),
            ActionStatus::Success("Mounted /dev/sdb1 at /media/u/USB".into())
        );
        assert_eq!(
            outcome_status(&request, true, None),
            ActionStatus::Success("Mounted /dev/sdb1 at ?".into())
        );
        match outcome_status(&request, false, None) {
            ActionStatus::Error(message) => {
                assert!(
                    message.contains("Mount failed for /dev/sdb1"),
                    "{}",
                    message
                );
                assert!(message.contains("not authorized"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn finish_status_mentions_force_hint_only_when_busy() {
        let mut request = InteractiveRequest {
            program: "sudo".into(),
            args: vec!["umount".into(), "/dev/sdb1".into()],
            op: InteractiveOp::Unmount,
            device_name: "sdb1".into(),
            context: "udisksctl: target is busy".into(),
            fallback_args: None,
            mount_target: None,
        };
        match outcome_status(&request, false, None) {
            ActionStatus::Error(message) => {
                assert!(message.contains("Press 'f'"), "{}", message)
            }
            other => panic!("expected error, got {:?}", other),
        }
        request.context = "udisksctl: permission denied".into();
        match outcome_status(&request, false, None) {
            ActionStatus::Error(message) => {
                assert!(!message.contains("Press 'f'"), "{}", message)
            }
            other => panic!("expected error, got {:?}", other),
        }
        assert_eq!(
            outcome_status(&request, true, None),
            ActionStatus::Success("Unmounted /dev/sdb1".into())
        );
    }

    #[test]
    fn finish_status_for_force_unmount() {
        let request = InteractiveRequest {
            program: "sudo".into(),
            args: vec!["umount".into(), "-l".into(), "/dev/sdb1".into()],
            op: InteractiveOp::ForceUnmount,
            device_name: "sdb1".into(),
            context: String::new(),
            fallback_args: Some(vec!["umount".into(), "-l".into(), "/media/u/USB".into()]),
            mount_target: None,
        };
        assert!(matches!(
            outcome_status(&request, true, None),
            ActionStatus::Success(_)
        ));
        assert!(matches!(
            outcome_status(&request, false, None),
            ActionStatus::Error(_)
        ));
    }

    const UNMOUNTED_JSON: &str = r#"{"blockdevices":[{"name":"sdb1","size":"8G","type":"part","fstype":"vfat","mountpoint":null,"mountpoints":[null],"label":"USBKEY","ro":false,"rm":true,"model":null,"vendor":null,"tran":"usb","serial":null,"uuid":null}]}"#;

    const MOUNTED_JSON: &str = r#"{"blockdevices":[{"name":"sdb1","size":"8G","type":"part","fstype":"vfat","mountpoint":"/media/u/USB","mountpoints":["/media/u/USB"],"label":"USBKEY","ro":false,"rm":true,"model":null,"vendor":null,"tran":"usb","serial":null,"uuid":null}]}"#;

    #[test]
    fn mount_success_reports_the_real_mountpoint() {
        let runner = FakeRunner::new()
            .push_ok("Mounted /dev/sdb1 at /media/u/USB.")
            .push_ok(MOUNTED_JSON);

        let mut app = app_with(vec![bd("sdb1", "part", Some("vfat"), None)]);
        app.list_state.select(Some(0));

        assert!(app.mount_selected(&runner).is_none());
        assert_eq!(
            app.action,
            ActionStatus::Success("Mounted /dev/sdb1 at /media/u/USB".into())
        );

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            CommandSpec::new("udisksctl", &["mount", "-b", "/dev/sdb1"])
        );
        assert_eq!(calls[1].program, "lsblk");

        // The tree was reloaded, so the device now shows the real mount point.
        assert!(disk::is_mounted(&app.selected_device().unwrap()));
    }

    #[test]
    fn unmount_busy_error_lists_the_processes_without_sudo() {
        let runner = FakeRunner::new()
            .push_fail("Error unmounting /dev/sdb1: target is busy (udisks-error-quark, 26)")
            .push_ok("                     USER        PID ACCESS COMMAND\n/dev/sdb1:           root       1234 ..c.. bash");

        let mut app = app_with(vec![with_mounts(
            bd("sdb1", "part", Some("ext4"), None),
            &["/mnt/data"],
        )]);
        app.list_state.select(Some(0));

        assert!(
            app.unmount_selected(&runner).is_none(),
            "the default mode produces no interactive request"
        );
        assert_eq!(
            runner.call_count(),
            2,
            "udisksctl then the busy diagnostics"
        );
        assert_eq!(
            runner.calls()[1],
            CommandSpec::new("fuser", &["-vm", "/dev/sdb1"])
        );
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(
                    message.contains("Unmount failed for /dev/sdb1"),
                    "{}",
                    message
                );
                assert!(message.contains("busy"), "{}", message);
                assert!(message.contains("1234"), "{}", message);
                assert!(message.contains("Press 'f'"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn unmount_failure_without_busy_skips_busy_diagnostics() {
        let runner = FakeRunner::new().push_fail("Error: permission denied");

        let mut app = app_with(vec![with_mounts(
            bd("sdb1", "part", Some("ext4"), None),
            &["/mnt/data"],
        )]);
        app.list_state.select(Some(0));

        assert!(app.unmount_selected(&runner).is_none());
        // Only udisksctl ran: fuser/lsof are not consulted when it is not busy.
        assert_eq!(runner.call_count(), 1);
        match &app.action {
            ActionStatus::Error(message) => {
                assert!(message.contains("permission denied"), "{}", message);
                assert!(!message.contains("Press 'f'"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
    }

    #[test]
    fn sudo_unmount_is_an_interactive_request_without_udisksctl() {
        let root = TempDir::new("sudo-umount");
        let runner = no_commands();
        let mut app = sudo_app(
            vec![with_mounts(
                bd("sdb1", "part", Some("ext4"), None),
                &["/mnt/data"],
            )],
            root.path(),
        );
        app.list_state.select(Some(0));

        let request = app.unmount_selected(&runner).expect("sudo umount request");
        assert_eq!(request.program, "sudo");
        assert_eq!(request.op, InteractiveOp::Unmount);
        assert_eq!(request.args, vec!["umount".to_string(), "/dev/sdb1".into()]);
        assert_eq!(request.mount_target, None);
        assert_eq!(runner.call_count(), 0, "sudo mode skips udisksctl");
    }

    #[test]
    fn unmount_success_reloads_the_tree() {
        let runner = FakeRunner::new()
            .push_ok("Unmounted /dev/sdb1.")
            .push_ok(UNMOUNTED_JSON);

        let mut app = app_with(vec![with_mounts(
            bd("sdb1", "part", Some("vfat"), None),
            &["/media/u/USB"],
        )]);
        app.list_state.select(Some(0));

        assert!(app.unmount_selected(&runner).is_none());
        assert_eq!(
            app.action,
            ActionStatus::Success("Unmounted /dev/sdb1".into())
        );
        assert!(!disk::is_mounted(&app.selected_device().unwrap()));
        assert_eq!(runner.call_count(), 2);
    }

    #[test]
    fn refresh_reports_failures_in_the_status_bar() {
        let runner = FakeRunner::new()
            .push_fail("lsblk: unknown column: MOUNTPOINTS")
            .push_fail("lsblk: no devices");

        let mut app = app_with(vec![bd("sda", "disk", None, None)]);
        app.refresh(&runner);

        match &app.action {
            ActionStatus::Error(message) => {
                assert!(message.starts_with("Refresh failed:"), "{}", message);
                assert!(message.contains("lsblk: no devices"), "{}", message);
            }
            other => panic!("expected error, got {:?}", other),
        }
        assert_eq!(runner.call_count(), 2);
    }

    #[test]
    fn refresh_success_replaces_the_device_tree() {
        let runner = FakeRunner::new().push_ok(MOUNTED_JSON);
        let mut app = app_with(vec![bd("sda", "disk", None, None)]);

        app.refresh(&runner);

        assert_eq!(app.devices.len(), 1);
        assert_eq!(app.devices[0].name, "sdb1");
        // A successful manual refresh clears a stale status instead of leaving
        // the old message (e.g. an earlier error) on screen.
        assert_eq!(app.action, ActionStatus::Success("Refreshed".into()));
        assert_eq!(runner.call_count(), 1);
    }

    #[test]
    fn refresh_success_clears_a_previous_error() {
        let runner = FakeRunner::new()
            .push_fail("lsblk: no devices")
            .push_fail("lsblk: no devices")
            .push_ok(MOUNTED_JSON);
        let mut app = app_with(vec![bd("sda", "disk", None, None)]);

        app.refresh(&runner);
        assert!(matches!(app.action, ActionStatus::Error(_)));

        app.refresh(&runner);
        assert_eq!(app.action, ActionStatus::Success("Refreshed".into()));
        assert_eq!(runner.call_count(), 3);
    }

    #[test]
    fn auto_refresh_only_returns_true_when_the_tree_changed() {
        let parse_runner = FakeRunner::new().push_ok(UNMOUNTED_JSON);
        let devices = disk::get_block_devices(&parse_runner).unwrap();
        let mut app = App::new(devices);

        // Identical tree: no redraw.
        let same = FakeRunner::new().push_ok(UNMOUNTED_JSON);
        assert!(!app.auto_refresh(&same));

        // The device got mounted in the meantime: redraw.
        let changed = FakeRunner::new().push_ok(MOUNTED_JSON);
        assert!(app.auto_refresh(&changed));
        assert!(disk::is_mounted(&app.selected_device().unwrap()));

        // Both lsblk attempts fail: stay silent and keep the old tree.
        let failing = FakeRunner::new()
            .push_fail("lsblk: unknown column: MOUNTPOINTS")
            .push_fail("lsblk: no devices");
        assert!(!app.auto_refresh(&failing));
        assert!(disk::is_mounted(&app.selected_device().unwrap()));
        assert!(matches!(app.action, ActionStatus::None));
    }

    #[test]
    fn finish_uses_runner_to_report_the_real_mountpoint() {
        let runner = FakeRunner::new().push_ok(MOUNTED_JSON);
        let mut app = app_with(vec![bd("sdb1", "part", Some("vfat"), None)]);
        let request = InteractiveRequest {
            program: "sudo".into(),
            args: vec![
                "mount".into(),
                "/dev/sdb1".into(),
                "/home/u/mnt/sdb1".into(),
            ],
            op: InteractiveOp::Mount,
            device_name: "sdb1".into(),
            context: "udisksctl: not authorized".into(),
            fallback_args: None,
            mount_target: None,
        };

        app.finish(&runner, request, true);

        assert_eq!(
            app.action,
            ActionStatus::Success("Mounted /dev/sdb1 at /media/u/USB".into())
        );
        assert_eq!(runner.call_count(), 1);
    }

    // --- B2: loop filter, search, details scroll -----------------------------

    /// A loop device as `lsblk` reports it (snap image / mounted ISO).
    fn loop_device(name: &str) -> BlockDevice {
        let mut device = bd(name, "loop", Some("squashfs"), None);
        device.ro = true;
        device
    }

    /// Every searchable field is carried by exactly one device here.
    ///
    /// `sda` (model SYSDISK) → `sda1` (ext4, /boot);
    /// `sdb` (model USBSTICK) → `sdb1` (label USBKEY, vfat, /media/u/USBKEY)
    /// → `sdb1-crypt` (crypt, ext4).
    fn search_tree() -> Vec<BlockDevice> {
        let mut system = bd("sda", "disk", None, None);
        system.model = Some("SYSDISK".into());
        let mut usb = bd("sdb", "disk", None, None);
        usb.model = Some("USBSTICK".into());
        let mut partition = bd("sdb1", "part", Some("vfat"), None);
        partition.label = Some("USBKEY".into());
        partition.mountpoints = vec!["/media/u/USBKEY".into()];
        let partition = with_children(
            partition,
            vec![bd("sdb1-crypt", "crypt", Some("ext4"), None)],
        );
        vec![
            with_children(
                system,
                vec![bd("sda1", "part", Some("ext4"), Some("/boot"))],
            ),
            with_children(usb, vec![partition]),
        ]
    }

    fn names(app: &App) -> Vec<String> {
        app.flat_items()
            .into_iter()
            .map(|item| item.device.name)
            .collect()
    }

    fn depths(app: &App) -> Vec<(String, usize)> {
        app.flat_items()
            .into_iter()
            .map(|item| (item.device.name, item.depth))
            .collect()
    }

    /// Type `query` into the search line and apply it, exactly like `/` … Enter.
    fn search_for(app: &mut App, query: &str) {
        app.begin_search();
        for character in query.chars() {
            app.search_push_char(character);
        }
        app.confirm_search();
    }

    #[test]
    fn loop_devices_are_hidden_by_default_and_toggle_with_l() {
        let mut app = app_with(vec![
            bd("sda", "disk", None, None),
            loop_device("loop0"),
            bd("sdb1", "part", Some("vfat"), Some("/mnt/x")),
        ]);
        assert!(app.hide_loops);
        assert_eq!(names(&app), vec!["sda", "sdb1"]);

        app.toggle_loops();
        assert!(!app.hide_loops);
        assert_eq!(names(&app), vec!["sda", "loop0", "sdb1"]);
        assert_eq!(
            app.action,
            ActionStatus::Success("Loop devices shown".into())
        );

        app.toggle_loops();
        assert!(app.hide_loops);
        assert_eq!(names(&app), vec!["sda", "sdb1"]);
        assert_eq!(
            app.action,
            ActionStatus::Success("Loop devices hidden".into())
        );
    }

    #[test]
    fn hidden_loop_devices_are_not_searchable_either() {
        let mut app = app_with(vec![bd("sda", "disk", None, None), loop_device("loop0")]);

        search_for(&mut app, "squashfs");
        assert!(
            app.flat_items().is_empty(),
            "a hidden loop device cannot match"
        );

        app.toggle_loops();
        assert_eq!(names(&app), vec!["loop0"]);
    }

    #[test]
    fn search_matches_name_label_model_fstype_and_mountpoints() {
        let cases: [(&str, Vec<&str>); 6] = [
            // Lowercase query, uppercase label: case-insensitive.
            ("usbkey", vec!["sdb", "sdb1", "sdb1-crypt"]),
            ("USBKEY", vec!["sdb", "sdb1", "sdb1-crypt"]),
            ("usbstick", vec!["sdb", "sdb1", "sdb1-crypt"]),
            ("vfat", vec!["sdb", "sdb1", "sdb1-crypt"]),
            ("/media/u", vec!["sdb", "sdb1", "sdb1-crypt"]),
            ("sda1", vec!["sda", "sda1"]),
        ];

        for (query, expected) in cases {
            let mut app = app_with(search_tree());
            search_for(&mut app, query);
            assert_eq!(names(&app), expected, "query {:?}", query);
        }
    }

    #[test]
    fn search_keeps_the_whole_subtree_of_a_matching_device() {
        let mut app = app_with(search_tree());
        // The model belongs to the disk itself: everything below it stays.
        search_for(&mut app, "SYSDISK");
        assert_eq!(names(&app), vec!["sda", "sda1"]);

        // `sdb1-crypt` is not matched by the query but is a descendant of the
        // matching `sdb1` (label USBKEY), so it is kept.
        search_for(&mut app, "usbkey");
        assert_eq!(names(&app), vec!["sdb", "sdb1", "sdb1-crypt"]);
        assert_eq!(
            depths(&app),
            vec![
                ("sdb".to_string(), 0),
                ("sdb1".to_string(), 1),
                ("sdb1-crypt".to_string(), 2),
            ]
        );
    }

    #[test]
    fn search_keeps_the_ancestors_of_a_matching_descendant() {
        let mut app = app_with(search_tree());
        search_for(&mut app, "crypt");

        // Only the branch leading to the match is kept, the depths survive.
        assert_eq!(
            depths(&app),
            vec![
                ("sdb".to_string(), 0),
                ("sdb1".to_string(), 1),
                ("sdb1-crypt".to_string(), 2),
            ]
        );
        assert!(!names(&app).contains(&"sda".to_string()));
    }

    #[test]
    fn search_ignores_the_collapsed_state() {
        let mut app = app_with(search_tree());
        app.list_state.select(Some(2)); // sdb
        app.toggle_collapse();
        assert!(app.is_collapsed("sdb"));
        assert_eq!(names(&app), vec!["sda", "sda1", "sdb"]);

        search_for(&mut app, "usbkey");
        assert_eq!(names(&app), vec!["sdb", "sdb1", "sdb1-crypt"]);

        // Clearing the query brings the collapsed tree back.
        app.clear_search();
        assert_eq!(names(&app), vec!["sda", "sda1", "sdb"]);
    }

    #[test]
    fn search_without_matches_empties_the_list_and_the_selection() {
        let mut app = app_with(search_tree());
        search_for(&mut app, "no-such-device");

        assert!(app.flat_items().is_empty());
        assert_eq!(app.list_state.selected(), None);
        assert!(app.selected_device().is_none());
    }

    #[test]
    fn search_clamps_the_selection_into_the_filtered_list() {
        let mut app = app_with(search_tree());
        app.list_state.select(Some(4)); // sdb1-crypt, the last row

        search_for(&mut app, "sda");
        assert_eq!(names(&app), vec!["sda", "sda1"]);
        assert_eq!(app.list_state.selected(), Some(1));
        assert_eq!(app.selected_device().unwrap().name, "sda1");
    }

    #[test]
    fn search_input_methods_build_and_drop_the_query() {
        let mut app = app_with(sample_tree());
        app.search = "stale".into();

        // `/` starts empty, even with an old query around.
        app.begin_search();
        assert_eq!(app.search, "");
        assert!(app.search_editing);

        for character in "sdb".chars() {
            app.search_push_char(character);
        }
        assert_eq!(app.search, "sdb");

        app.search_backspace();
        assert_eq!(app.search, "sd");

        // Backspace on an empty query is a no-op, not a panic.
        app.search_backspace();
        app.search_backspace();
        app.search_backspace();
        assert_eq!(app.search, "");

        app.search_push_char('s');
        app.confirm_search();
        assert_eq!(app.search, "s");
        assert!(!app.search_editing, "Enter leaves the input line");
        assert!(app.filter_active(), "and keeps the query applied");

        // Esc in the input line drops the query…
        app.begin_search();
        app.search_push_char('x');
        app.cancel_search();
        assert_eq!(app.search, "");
        assert!(!app.search_editing);
        assert!(!app.filter_active());

        // …while Esc on the list only drops an applied query.
        search_for(&mut app, "sda");
        assert!(app.filter_active());
        app.clear_search();
        assert_eq!(app.search, "");
        assert!(!app.search_editing);
    }

    #[test]
    fn search_changes_drop_the_force_confirmation_and_the_details_scroll() {
        let mut app = forceable_app();
        app.force_unmount_selected(&no_commands());
        assert!(app.force_confirm.is_some());
        app.details_scroll = 7;

        // Typing already re-filters the list: the confirmation and the scroll
        // offset belong to the previous view.
        app.begin_search();
        app.search_push_char('s');
        assert_eq!(app.force_confirm, None);
        assert!(!force_prompt_shown(&app));
        assert_eq!(app.details_scroll, 0);

        // Toggling the loop filter does the same.
        app.force_unmount_selected(&no_commands());
        app.details_scroll = 3;
        app.toggle_loops();
        assert_eq!(app.force_confirm, None);
        assert_eq!(app.details_scroll, 0);
    }

    #[test]
    fn details_scroll_is_bounded_and_resets_when_the_selection_moves() {
        let mut app = app_with(sample_tree());

        app.scroll_details_up();
        assert_eq!(app.details_scroll, 0, "a fresh view cannot scroll up");

        for _ in 0..3 {
            app.scroll_details_down();
        }
        assert_eq!(app.details_scroll, 3);
        app.scroll_details_up();
        assert_eq!(app.details_scroll, 2);

        // The cap keeps u16 arithmetic harmless however long a key is held.
        for _ in 0..200 {
            app.scroll_details_down();
        }
        assert_eq!(app.details_scroll, 100);

        app.navigate_down();
        assert_eq!(
            app.details_scroll, 0,
            "moving the selection resets the scroll"
        );

        app.details_scroll = 5;
        app.set_devices(sample_tree());
        assert_eq!(
            app.details_scroll, 5,
            "a refresh that keeps the selection keeps the scroll offset"
        );
        app.set_devices(vec![bd("sdc", "disk", None, None)]);
        assert_eq!(
            app.details_scroll, 0,
            "a refresh that moves the selection resets it"
        );
    }

    /// Redirect `HOME` to a scratch directory for the duration of `f`, so the
    /// owner options of a sudo mount and the default mount root never touch the
    /// real home directory.
    ///
    /// `HOME` is process-global, so the environment lock serialises every test
    /// that uses this helper.
    fn with_temp_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());

        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let home = std::env::temp_dir().join(format!(
            "mount-tui-app-test-{}-{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&home).unwrap();

        let previous = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let result = f(&home);
        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&home);
        result
    }
}
