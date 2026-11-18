use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, List, ListDirection, ListItem, Paragraph, Widget, Wrap},
    Frame,
};

use crate::app::{ActionStatus, App};
use crate::disk::{self, BlockDevice};

fn selected_style() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(Color::LightBlue)
        .add_modifier(Modifier::BOLD)
}

fn usb_style() -> Style {
    Style::new()
        .fg(Color::LightGreen)
        .add_modifier(Modifier::BOLD)
}

fn mounted_style() -> Style {
    Style::new().fg(Color::Cyan)
}

fn protected_style() -> Style {
    Style::new().fg(Color::LightRed)
}

fn normal_style() -> Style {
    Style::new().fg(Color::White)
}

/// Compact device tags shown in the list, in fixed order:
/// `[USB]`, `[RM]`, `[RO]`, `[LABEL]`. Every tag carries a leading space.
fn device_tags(device: &BlockDevice) -> String {
    let mut tags = String::new();
    if disk::is_usb(device) {
        tags.push_str(" [USB]");
    }
    if device.rm {
        tags.push_str(" [RM]");
    }
    if device.ro {
        tags.push_str(" [RO]");
    }
    if let Some(label) = device.label.as_deref().filter(|label| !label.is_empty()) {
        tags.push_str(&format!(" [{}]", label));
    }
    tags
}

/// List fragment for the mount state: first mount point plus `(+N)` when the
/// device has more than one.
fn mount_summary(device: &BlockDevice) -> String {
    let Some((first, rest)) = device.mountpoints.split_first() else {
        return String::new();
    };
    if rest.is_empty() {
        format!(" -> {}", first)
    } else {
        format!(" -> {} (+{})", first, rest.len())
    }
}

/// Message and style of the status bar. Rendering and height estimation share
/// this single source so they can never disagree.
fn status_content(action: &ActionStatus) -> (String, Style) {
    let style = match action {
        ActionStatus::None => normal_style(),
        other => Style::new().fg(status_color(other)),
    };
    let message = match action {
        ActionStatus::None => {
            "Ready. Select a device and press 'm' to mount or 'u' to unmount.".to_string()
        }
        ActionStatus::Mounting => "Mounting...".to_string(),
        ActionStatus::Unmounting => "Unmounting...".to_string(),
        ActionStatus::Success(message) | ActionStatus::Error(message) => message.clone(),
    };
    (message, style)
}

fn status_color(action: &ActionStatus) -> Color {
    match action {
        ActionStatus::Error(_) => Color::Red,
        ActionStatus::Success(_) => Color::Green,
        ActionStatus::Mounting | ActionStatus::Unmounting => Color::Yellow,
        ActionStatus::None => Color::DarkGray,
    }
}

/// Status bar height (borders included) for `message` rendered into `width`
/// columns.
///
/// The message is rendered with the exact same widget / wrap settings as
/// [`render_action_status`] into a scratch buffer, then the last row carrying
/// content is located. Measuring the real rendering (instead of estimating)
/// keeps wide characters and word wrapping honest; internal blank lines are
/// part of the message and therefore counted (`last_content_row + 1`).
///
/// The scratch buffer is 60 rows tall so that content after a long run of
/// blank lines is still seen; the final height is capped at 8 any way.
fn status_bar_height(message: &str, width: u16) -> u16 {
    /// Measurement window; larger than the 8 row cap so blank-line runs cannot
    /// hide later content. Costs a `60 × width` scratch buffer per draw.
    const MEASURE_ROWS: u16 = 60;

    let width = width.max(1);
    let area = Rect::new(0, 0, width, MEASURE_ROWS);
    let mut buffer = Buffer::empty(area);
    Paragraph::new(message)
        .wrap(Wrap { trim: true })
        .render(area, &mut buffer);

    let rows = buffer
        .content()
        .chunks(usize::from(area.width))
        .rposition(|row| row.iter().any(|cell| cell.symbol() != " "))
        .map_or(0, |last_row| last_row + 1);
    (u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(2)).clamp(3, 8)
}

/// Rows of the help bar (borders + two text lines).
const HELP_HEIGHT: u16 = 4;

/// Terminal height from which the help bar is shown at all.
const HELP_MIN_TERMINAL_HEIGHT: u16 = 14;

/// Rows the list / details panes always keep, the status bar is capped below.
const CONTENT_MIN_HEIGHT: u16 = 6;

/// Smallest possible status bar: two borders plus one text line.
const STATUS_MIN_HEIGHT: u16 = 3;

/// Help bar height for a terminal `terminal_height` rows tall.
///
/// On a short terminal the device list is more valuable than the key hints, so
/// the bar disappears entirely (0) instead of squeezing the content area.
fn help_height(terminal_height: u16) -> u16 {
    if terminal_height >= HELP_MIN_TERMINAL_HEIGHT {
        HELP_HEIGHT
    } else {
        0
    }
}

/// Final status bar height: the measured text height, capped so that the title
/// (1 row), the help bar and a [`CONTENT_MIN_HEIGHT`] row content area always
/// fit into `terminal_height`. Never below [`STATUS_MIN_HEIGHT`].
fn status_height(measured: u16, terminal_height: u16) -> u16 {
    let available =
        terminal_height.saturating_sub(1 + help_height(terminal_height) + CONTENT_MIN_HEIGHT);
    measured.min(available).max(STATUS_MIN_HEIGHT)
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let (status_message, _) = status_content(&app.action);
    // A border on each side leaves `width - 2` text columns.
    let measured_status = status_bar_height(&status_message, area.width.saturating_sub(2));
    let status_rows = status_height(measured_status, area.height);
    let help_rows = help_height(area.height);

    // Main layout: title | content | status | help
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),           // title
            Constraint::Min(0),              // content
            Constraint::Length(status_rows), // action status (dynamic)
            Constraint::Length(help_rows),   // help bar (hidden on short terminals)
        ])
        .split(area);

    // Title
    let title = Paragraph::new(" Mount-TUI — Disk & USB Manager ").style(
        Style::new()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(title, main_layout[0]);

    // Content: split into left (device list) and right (details)
    let content_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(main_layout[1]);

    render_device_list(frame, app, content_layout[0]);
    render_device_details(frame, app, content_layout[1]);

    // Action status bar
    render_action_status(frame, app, main_layout[2]);

    // Help bar (absent on short terminals, the layout gives it no rows then)
    if help_rows > 0 {
        render_help(frame, app, main_layout[3]);
    }
}

fn render_device_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Devices ")
        .border_style(Style::new().fg(Color::Cyan));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let list_items: Vec<ListItem> = app
        .flat_items()
        .into_iter()
        .map(|item| {
            // A search query expands every branch so matches are visible; the
            // marker follows what is actually on screen, not the collapse
            // state that only applies to the unfiltered tree.
            let shows_children = !app.is_collapsed(&item.device.name) || app.filter_active();
            let prefix = if App::has_children(&item.device) {
                if shows_children {
                    " [v] "
                } else {
                    " [>] "
                }
            } else {
                "     "
            };

            let tags = device_tags(&item.device);
            let mount_info = mount_summary(&item.device);

            let indent = "  ".repeat(item.depth);

            let fs_info = match &item.device.fstype {
                Some(fs) => format!(" [{}]", fs),
                None => String::new(),
            };

            let model_info = if item.depth == 0 && item.device.model.is_some() {
                format!(" ({})", item.device.model.as_deref().unwrap_or(""))
            } else {
                String::new()
            };

            let text = format!(
                "{}{}{:<8} {:>7}{}{}{}{}",
                indent,
                prefix,
                item.device.name,
                item.device.size,
                tags,
                fs_info,
                mount_info,
                model_info,
            );

            // The selection highlight is drawn by `List::highlight_style`.
            let style = if disk::is_usb(&item.device) {
                usb_style()
            } else if disk::is_mounted(&item.device) {
                mounted_style()
            } else {
                normal_style()
            };

            ListItem::new(Line::styled(text, style))
        })
        .collect();

    let list = List::new(list_items)
        .direction(ListDirection::TopToBottom)
        .highlight_style(selected_style());

    // Stateful rendering keeps the selection scrolled into view.
    frame.render_stateful_widget(list, inner, &mut app.list_state);
}

fn render_device_details(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Details ")
        .border_style(Style::new().fg(Color::Cyan));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(device) = app.selected_device() else {
        let text = Paragraph::new("No device selected").style(normal_style());
        frame.render_widget(text, inner);
        return;
    };

    let mut lines = Vec::new();

    lines.push(Line::styled(
        format!("Device:  {}", device.name),
        Style::new().add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::from(format!("Type:    {}", device.device_type)));
    lines.push(Line::from(format!("Size:    {}", device.size)));

    if let Some(ref v) = device.vendor {
        lines.push(Line::from(format!("Vendor:  {}", v)));
    }
    if let Some(ref m) = device.model {
        lines.push(Line::from(format!("Model:   {}", m)));
    }
    if let Some(ref t) = device.tran {
        lines.push(Line::from(format!("Transport: {}", t)));
    }
    if let Some(ref s) = device.serial {
        lines.push(Line::from(format!("Serial:  {}", s)));
    }
    lines.push(Line::from(format!(
        "USB:     {}",
        if disk::is_usb(&device) { "Yes" } else { "No" }
    )));

    lines.push(Line::from(""));
    if let Some(ref fs) = device.fstype {
        lines.push(Line::from(format!("FS Type: {}", fs)));
    } else {
        lines.push(Line::from("FS Type: None"));
    }
    if let Some(ref label) = device.label {
        lines.push(Line::from(format!("Label:   {}", label)));
    }
    if disk::is_mounted(&device) {
        for (index, mountpoint) in device.mountpoints.iter().enumerate() {
            let label = if index == 0 { "Mounted: " } else { "         " };
            lines.push(Line::styled(
                format!("{}{}", label, mountpoint),
                Style::new().fg(Color::Cyan),
            ));
        }
    } else {
        lines.push(Line::from("Mounted: No"));
    }

    let system_line = if disk::is_system_mount(&device) {
        Line::styled("System:  yes (protected)", protected_style())
    } else {
        Line::from("System:  no")
    };
    lines.push(system_line);

    if let Some(ref u) = device.uuid {
        lines.push(Line::from(format!("UUID:    {}", u)));
    }
    if let Some(ref p) = device.children {
        lines.push(Line::from(format!("Partitions: {}", p.len())));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(format!("Path:    /dev/{}", device.name)));

    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .style(normal_style())
        // `[` / `]` scroll the rendered details; long values wrap instead of
        // being cut, so scrolling reaches every row.
        .scroll((app.details_scroll, 0));

    frame.render_widget(paragraph, inner);
}

fn render_action_status(frame: &mut Frame, app: &mut App, area: Rect) {
    let (message, style) = status_content(&app.action);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(status_color(&app.action)))
        .title(" Status ");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = Paragraph::new(message)
        .style(style)
        .wrap(Wrap { trim: true });
    frame.render_widget(text, inner);
}

fn render_help(frame: &mut Frame, app: &App, area: Rect) {
    let key = |label: &'static str| Span::styled(label, Style::new().fg(Color::Yellow));
    let hint = |text: &'static str| Span::styled(text, Style::new().fg(Color::DarkGray));

    // First line: what the list / search line is doing right now.
    let first_line = if app.search_editing {
        vec![
            Span::styled(" Search: ", Style::new().fg(Color::Yellow)),
            Span::raw(app.search.clone()),
        ]
    } else if app.search.is_empty() {
        vec![
            key(" <Up/Down PgUp/PgDn> "),
            Span::raw("Navigate  "),
            key(" <Left/Right> "),
            Span::raw("Collapse  "),
            key(" / "),
            Span::raw("Search  "),
            key(" l "),
            Span::raw("Loops"),
        ]
    } else {
        vec![
            Span::styled(" Filter: ", Style::new().fg(Color::Yellow)),
            Span::raw(app.search.clone()),
            hint(" (/ edit, Esc clear)"),
        ]
    };

    let help_text = Text::from(vec![
        Line::from(first_line),
        Line::from(vec![
            key(" m "),
            Span::raw("Mount  "),
            key(" u "),
            Span::raw("Unmount  "),
            Span::styled(" f ", Style::new().fg(Color::Red)),
            Span::raw("Force unmount  "),
            key(" [ ] "),
            Span::raw("Details  "),
            key(" r "),
            Span::raw("Refresh  "),
            key(" q "),
            Span::raw("Quit"),
        ]),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let paragraph = Paragraph::new(help_text).style(normal_style());
    frame.render_widget(paragraph, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bd(name: &str, fstype: Option<&str>, mountpoints: &[&str]) -> BlockDevice {
        BlockDevice {
            name: name.to_string(),
            size: "1G".to_string(),
            device_type: "part".to_string(),
            fstype: fstype.map(str::to_string),
            mountpoints: mountpoints.iter().map(|mp| mp.to_string()).collect(),
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

    #[test]
    fn device_tags_follow_fixed_order() {
        let mut device = bd("sdb1", Some("vfat"), &["/media/u/USB"]);
        device.tran = Some("usb".to_string());
        device.rm = true;
        device.ro = true;
        device.label = Some("USBKEY".to_string());
        assert_eq!(device_tags(&device), " [USB] [RM] [RO] [USBKEY]");

        let mut labelled = bd("sdc1", Some("ext4"), &[]);
        labelled.label = Some("DATA".to_string());
        assert_eq!(device_tags(&labelled), " [DATA]");
        assert_eq!(device_tags(&bd("sda", None, &[])), "");
    }

    #[test]
    fn mount_summary_shows_first_mountpoint_plus_extra_count() {
        assert_eq!(mount_summary(&bd("sda1", Some("ext4"), &[])), "");
        assert_eq!(
            mount_summary(&bd("sda1", Some("ext4"), &["/mnt/a"])),
            " -> /mnt/a"
        );
        assert_eq!(
            mount_summary(&bd("sda1", Some("ext4"), &["/mnt/a", "/mnt/b", "/mnt/c"])),
            " -> /mnt/a (+2)"
        );
    }

    #[test]
    fn status_bar_height_grows_with_message_and_clamps() {
        assert_eq!(status_bar_height("short", 100), 3);
        assert_eq!(status_bar_height("", 40), 3);
        // 80 chars in a 20 column pane estimate to 4 rows (4 + 2 borders).
        assert_eq!(status_bar_height(&"x".repeat(80), 20), 6);
        // Very long text stays bounded at 8 rows.
        assert_eq!(status_bar_height(&"x".repeat(1000), 10), 8);
        // Explicit newlines (busy hint) count as separate rows.
        assert_eq!(status_bar_height("one\ntwo\nthree", 40), 5);
        // Zero width must not panic (treated as one column).
        assert_eq!(status_bar_height("abc", 0), 5);
    }

    #[test]
    fn status_bar_height_matches_real_rendering_for_wide_text() {
        // 8 CJK characters (16 columns) wrap onto 2 rows in an 8 column pane.
        assert_eq!(status_bar_height("中文字符宽度测试", 8), 4);

        // Long CJK error with a trailing marker: word wrapping needs more rows
        // than a width/chars estimate would predict (160 display columns =
        // 14 ASCII + 128 CJK + 18 ASCII → naive 4 rows at width 40, real
        // rendering 5).
        let long_error = format!(
            "Mount failed: {} END_OF_LONG_ERROR",
            "目标设备正忙请检查占用进程后重试".repeat(4)
        );
        assert_eq!(status_bar_height(&long_error, 40), 7);
        assert_eq!(status_bar_height(&long_error, 118), 5);

        // Zero width must not panic (one column assumed).
        assert_eq!(status_bar_height("中", 0), 3);
    }

    #[test]
    fn status_bar_height_counts_internal_blank_lines() {
        // The blank line between the two text lines belongs to the message:
        // 3 rendered rows + 2 borders.
        assert_eq!(status_bar_height("one\n\ntwo", 40), 5);
        // A message of blank lines only has no content → minimum height.
        assert_eq!(status_bar_height("\n\n", 40), 3);
        // Content after a long run of blank lines must still be measured; the
        // height then saturates at the 8 row cap (a fixed 8 row scratch window
        // would have seen only the first line and returned 3).
        assert_eq!(status_bar_height("a\n\n\n\n\n\n\n\n\n\nEND", 40), 8);
    }

    #[test]
    fn help_bar_is_hidden_on_short_terminals() {
        assert_eq!(help_height(1), 0);
        assert_eq!(help_height(13), 0);
        assert_eq!(help_height(14), 4);
        assert_eq!(help_height(40), 4);
    }

    #[test]
    fn status_height_reserves_a_six_row_content_area() {
        // Tall terminal: the measured height is used as is (up to its own cap).
        assert_eq!(status_height(3, 40), 3);
        assert_eq!(status_height(8, 40), 8);

        // 14 rows: 14 - 1 title - 4 help - 6 content = 3 rows for the status.
        assert_eq!(status_height(8, 14), 3);
        // 13 rows: no help bar, so 13 - 1 - 6 = 6 rows remain.
        assert_eq!(status_height(8, 13), 6);
        assert_eq!(status_height(3, 13), 3);

        // A tiny terminal cannot fit the content area: keep the minimum status.
        assert_eq!(status_height(8, 6), 3);
        assert_eq!(status_height(8, 0), 3);
    }

    #[test]
    fn status_content_matches_rendered_message() {
        let (message, _) = status_content(&ActionStatus::Success("Mounted".into()));
        assert_eq!(message, "Mounted");
        let (message, _) = status_content(&ActionStatus::Mounting);
        assert_eq!(message, "Mounting...");
        let (message, _) = status_content(&ActionStatus::Unmounting);
        assert_eq!(message, "Unmounting...");
        let (message, _) = status_content(&ActionStatus::Error("boom".into()));
        assert_eq!(message, "boom");
        let (message, _) = status_content(&ActionStatus::None);
        assert!(message.starts_with("Ready."), "{}", message);
    }
}
