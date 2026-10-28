# egui for the GUI

The GUI uses egui, not Tauri, iced or Slint. The treemap is custom drawing, which immediate mode makes simple. Lists of up to 10M results need virtualised rendering inside the process; Tauri would push millions of rows across an IPC boundary to a webview. A TUI was rejected because disk cleanup needs mouse-driven visual exploration.
