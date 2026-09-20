# Transient seats

A *seat* is a group of input devices (a keyboard, a pointer, ...) that share the same focus. Your own
keyboard and mouse make up the primary seat, named after your session (usually `seat0`).

`fht-compositor` implements [`ext-transient-seat-v1`](https://wayland.app/protocols/ext-transient-seat-v1), which lets
a program create additional `wl_seat`s that live only as long as the program keeps them around. Together with
`virtual-keyboard-unstable-v1` and `wlr-virtual-pointer-unstable-v1`, this allows programs to drive applications with
their own keyboard and mouse, **without touching yours**. Example use cases include remote desktop servers, automation
tools, and AI agents that operate a program next to you.

## How it works

1. The program creates a transient seat and gets a `wl_seat` global for it.
2. It creates a virtual keyboard and/or virtual pointer *on that seat*, and sends input through them.
3. Applications see this seat as any other `wl_seat`, with its own keyboard focus and pointer position.

The pointer of a transient seat starts in the middle of the active output. Clicking on a window with it gives that
window keyboard focus **for this seat only**, key presses through the virtual keyboard then go to that window.

## What transient seats do (and don't do)

Input from transient seats is only ever delivered to applications, it never changes what *you* see or do. Compared to
your own seat, transient seats:

- **Don't** activate, raise, or move windows, and don't change your active output or workspace.
- **Don't** trigger [keybindings](/configuration/keybindings), [mousebindings](/configuration/mousebindings), or
  [gesturebinds](/configuration/gesturebindings), and can't start interactive move/resize.
- **Don't** move your cursor, and don't get a cursor drawn (nor change how your cursor looks).
- **Don't** count as user activity for idle handling.
- Are completely **inert while the session is locked**.
- Can't activate windows through `xdg-activation`.

Since pointers of transient seats are placed on the visible windows of the space, they can only interact with windows that
are currently visible, IE. on an active workspace.

## Limitations

- A virtual pointer created on the primary seat (or without specifying a seat, which means the primary seat) is
  ignored, so programs can't steal your mouse this way. Virtual keyboards are handled by Smithay and work on any seat,
  including the primary one.
- A client can hold up to 8 transient seats at once, further requests get `denied`.
- Clients running inside a [security context](https://wayland.app/protocols/security-context-v1) (for example
  Flatpak applications) don't see the transient seat and virtual pointer globals.
