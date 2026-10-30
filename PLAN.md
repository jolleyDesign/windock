# Windock plan

Features Windock is missing, in the order they're being worked on. The first group is what keeps
`hide_taskbar = true` from being a full taskbar replacement; the rest is what people expect from a dock.

Status: ☐ not started · ◐ in progress · ☑ done

## 1. Replacing the taskbar

### ☑ 1.1 Show desktop
An optional item that minimizes every window, and puts them back on a second click (like the taskbar's
corner sliver or Win+D).
- `IShellDispatch4::ToggleDesktop` (Shell.Application) does the work and keeps Windows' own restore list.
- Config: `[dock] show_desktop = "auto" | "always" | "never"` (auto: while the taskbar is hidden).
  Lives after the clock/status icons in the dock or the corner island (in the dock when they're hidden).
- Also a "Show desktop" entry in the dock's right-click menu.

### ☑ 1.2 Start button
An item at the start of the dock that opens the Start menu; right-click opens the Win+X menu.
- Opens Start with a synthesized Win key (works with the taskbar hidden, and toggles), Win+X likewise.
- Icon: the Windows logo drawn as four squares, Windows blue.
- Config: `[dock] start_button = "auto" | "always" | "never"`.

### ☑ 1.3 Other apps' tray icons
Icons apps add with `Shell_NotifyIcon` (OneDrive, Slack, VPN clients…) disappear with the taskbar.
- `src/tray.rs`: a hidden, topmost `Shell_TrayWnd` on its own thread, so `Shell_NotifyIcon` reaches it.
  It keeps a copy of each icon and forwards every message (icon updates, app bar messages,
  WM_COMMANDs) to Explorer's tray, so Explorer stays in sync and quitting or crashing loses nothing.
  It mirrors the taskbar's rect for apps that look it up, and answers `Shell_NotifyIconGetRect`.
- Running apps re-add their icons when it broadcasts `TaskbarCreated` at start.
- Clicks send the owner the same messages Explorer does (v4 and legacy forms, double-click,
  WM_CONTEXTMENU), after `AllowSetForegroundWindow`.
- Windows 11's "show in taskbar" choice (`HKCU\Control Panel\NotifyIconSettings`, watched for
  changes) decides what's in the dock; the rest are in a grid flyout behind an arrow
  (`src/app/overflow.rs`), which closes on Esc or a click elsewhere.
  `[tray] apps = "promoted" | "all" | "none"`.
- Known gap: an app that ignores `TaskbarCreated` (OneDrive does) shows up only once it next
  updates its icon with its callback message, or is restarted. Balloon notifications are left to Windows.

## 2. Big dock features

### ☐ 2.1 Jump lists
Recent/pinned files and app tasks in the right-click menu.
- `IApplicationDocumentLists` for recent items, `IApplicationDestinations`/custom destinations via
  the shell's `IAutomaticDestinationList`/`CustomDestinationList` files per AppUserModelID.

### ☐ 2.2 Progress and badges
Download/copy progress bars and unread badges drawn on icons.
- Apps send `ITaskbarList3` progress to the real taskbar; investigate reading it (the taskbar's
  UI Automation tree, or our own `TaskbarButtonCreated` window) before committing.

### ☐ 2.3 Multiple monitors
`monitor = "all"` (a dock per monitor, optionally only that monitor's windows) and `"cursor"`
(the dock follows the mouse).

## 3. Smaller dock features

### ☐ 3.1 Folder stacks
A pinned folder fans out (or shows a grid) of its contents instead of only opening Explorer.

### ☐ 3.2 Recycle Bin
A dock item that shows empty/full, opens the bin, takes drops to delete, and has "Empty Recycle Bin".

### ☐ 3.3 Separators and spacers
User-placed dividers between pinned apps (`[[pinned]] separator = true`).

### ☐ 3.4 Virtual desktops
Desktop switcher and "Move to desktop" in a window's menu.

### ☐ 3.5 More menu items
Run as administrator, Open file location, Properties.

## 4. Quality

### ☐ 4.1 Accessibility
UI Automation provider for the dock items, and keyboard navigation (focus the dock, arrows, Enter).

### ☐ 4.2 Installer and updates
An installer/uninstaller (or winget manifest) and an update check.
