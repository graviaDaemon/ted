use std::io::{self, Write};
use crossterm::{
    cursor,
    event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    queue,
    terminal,
};

pub struct Tui {
    input_buf: String,
    cursor_pos: usize,
    prompt: &'static str,
    log_lines: Vec<String>,
}

// Layout (rows are 0-indexed):
//   0 .. rows-4  : log area  (rows-3 lines, scrolls via log_lines buffer)
//   rows-3       : separator ─────
//   rows-2       : input line
//   rows-1       : separator ─────

impl Tui {
    pub fn enter() -> io::Result<Tui> {
        terminal::enable_raw_mode()?;
        let tui = Tui {
            input_buf: String::new(),
            cursor_pos: 0,
            prompt: "> ",
            log_lines: Vec::new(),
        };
        let mut out = io::stdout();
        queue!(out, terminal::Clear(terminal::ClearType::All), cursor::MoveTo(0, 0))?;
        out.flush()?;
        tui.redraw();
        Ok(tui)
    }

    pub fn exit(&self) {
        let _ = terminal::disable_raw_mode();
        // Print a newline so the shell prompt appears on a fresh line after exit
        let _ = writeln!(io::stdout());
    }

    pub fn handle_resize(&self) {
        self.redraw();
    }

    pub fn handle_log(&mut self, line: &str) {
        self.log_lines.push(line.trim_end_matches('\n').to_string());
        self.redraw();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        // Only act on press events. Terminals on Windows also emit Release and
        // Repeat events for every keypress, which would double every character.
        if key.kind != KeyEventKind::Press {
            return None;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some("\x04".to_string());
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some("\x04".to_string());
            }
            KeyCode::Char(c) => {
                self.input_buf.insert(self.cursor_pos, c);
                self.cursor_pos += 1;
                self.redraw_input();
            }
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    self.input_buf.remove(self.cursor_pos);
                    self.redraw_input();
                }
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    self.reposition_cursor();
                }
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input_buf.len() {
                    self.cursor_pos += 1;
                    self.reposition_cursor();
                }
            }
            KeyCode::Home => {
                self.cursor_pos = 0;
                self.reposition_cursor();
            }
            KeyCode::End => {
                self.cursor_pos = self.input_buf.len();
                self.reposition_cursor();
            }
            KeyCode::Enter => {
                let cmd = self.input_buf.clone();
                self.input_buf.clear();
                self.cursor_pos = 0;
                self.redraw_input();
                return Some(cmd);
            }
            _ => {}
        }
        None
    }

    fn redraw(&self) {
        let Ok((cols, rows)) = terminal::size() else { return };
        if rows < 4 {
            return;
        }

        // Guard against terminals reporting the console buffer width instead of
        // the visible window width (common on Windows in some configurations).
        let cols = cols.min(500) as usize;
        let sep = "─".repeat(cols);

        let log_row_count = (rows - 3) as usize;
        let log_start = self.log_lines.len().saturating_sub(log_row_count);

        let mut out = io::stdout();

        // Paint log lines — one per row, filling from the top
        for row in 0..log_row_count {
            let _ = queue!(out, cursor::MoveTo(0, row as u16));
            let _ = write!(out, "\x1b[2K"); // erase full line
            if let Some(line) = self.log_lines.get(log_start + row) {
                let visible: String = line.chars().take(cols).collect();
                let _ = write!(out, "{}", visible);
            }
        }

        // Top separator
        let _ = queue!(out, cursor::MoveTo(0, rows - 3));
        let _ = write!(out, "{}", sep);

        // Input row
        let _ = queue!(out, cursor::MoveTo(0, rows - 2));
        let _ = write!(out, "\x1b[2K{}{}", self.prompt, self.input_buf);

        // Bottom separator
        let _ = queue!(out, cursor::MoveTo(0, rows - 1));
        let _ = write!(out, "{}", sep);

        // Leave cursor in the input line at the correct column
        let col = (self.prompt.len() + self.cursor_pos).min(cols) as u16;
        let _ = queue!(out, cursor::MoveTo(col, rows - 2));

        let _ = out.flush();
    }

    fn redraw_input(&self) {
        let Ok((cols, rows)) = terminal::size() else { return };
        if rows < 4 {
            return;
        }
        let cols = cols.min(500) as usize;
        let mut out = io::stdout();
        let _ = queue!(out, cursor::MoveTo(0, rows - 2));
        let _ = write!(out, "\x1b[2K{}{}", self.prompt, self.input_buf);
        let col = (self.prompt.len() + self.cursor_pos).min(cols) as u16;
        let _ = queue!(out, cursor::MoveTo(col, rows - 2));
        let _ = out.flush();
    }

    fn reposition_cursor(&self) {
        let Ok((cols, rows)) = terminal::size() else { return };
        if rows < 4 {
            return;
        }
        let cols = cols.min(500) as usize;
        let col = (self.prompt.len() + self.cursor_pos).min(cols) as u16;
        let mut out = io::stdout();
        let _ = queue!(out, cursor::MoveTo(col, rows - 2));
        let _ = out.flush();
    }
}
