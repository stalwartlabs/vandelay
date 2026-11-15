/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::{Display, Write as _};
use std::io::{self, Write};

use super::{Grouped, Styled, Tone};

const GUTTER: &str = "  ";
const INDENT: &str = "  ";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug)]
pub struct Column {
    pub title: &'static str,
    pub align: Align,
}

impl Column {
    pub const fn left(title: &'static str) -> Column {
        Column {
            title,
            align: Align::Left,
        }
    }

    pub const fn right(title: &'static str) -> Column {
        Column {
            title,
            align: Align::Right,
        }
    }
}

struct Cell {
    end: usize,
    width: usize,
    tone: Option<Tone>,
}

pub struct Table<'c> {
    columns: &'c [Column],
    text: String,
    cells: Vec<Cell>,
}

impl<'c> Table<'c> {
    pub fn new(columns: &'c [Column]) -> Table<'c> {
        Table {
            columns,
            text: String::new(),
            cells: Vec::new(),
        }
    }

    pub fn cell(&mut self, value: impl Display, tone: Option<Tone>) -> &mut Self {
        let start = self.text.len();
        let _ = write!(self.text, "{value}");
        let width = self.text.get(start..).map_or(0, |s| s.chars().count());
        self.cells.push(Cell {
            end: self.text.len(),
            width,
            tone,
        });
        self
    }

    pub fn rows(&self) -> usize {
        self.cells.len() / self.columns.len().max(1)
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cells.clear();
    }

    pub fn count(&mut self, n: u64, nonzero: Option<Tone>) -> &mut Self {
        let tone = if n == 0 { Some(Tone::Dim) } else { nonzero };
        self.cell(Grouped(n), tone)
    }

    pub fn write(&self, styled: Styled, out: &mut dyn Write) -> io::Result<()> {
        let n = self.columns.len().max(1);
        let mut widths: Vec<usize> = self.columns.iter().map(|c| c.title.len()).collect();
        for row in self.cells.chunks(n) {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.width);
            }
        }
        out.write_all(INDENT.as_bytes())?;
        for (i, (column, width)) in self.columns.iter().zip(&widths).enumerate() {
            Slot {
                align: column.align,
                fill: width.saturating_sub(column.title.len()),
                first: i == 0,
                last: i + 1 == n,
            }
            .write(out, styled, column.title, Some(Tone::Heading))?;
        }
        out.write_all(b"\n")?;
        let mut start = 0;
        for row in self.cells.chunks(n) {
            out.write_all(INDENT.as_bytes())?;
            for (i, ((column, width), cell)) in
                self.columns.iter().zip(&widths).zip(row).enumerate()
            {
                let text = self.text.get(start..cell.end).unwrap_or_default();
                start = cell.end;
                Slot {
                    align: column.align,
                    fill: width.saturating_sub(cell.width),
                    first: i == 0,
                    last: i + 1 == row.len(),
                }
                .write(out, styled, text, cell.tone)?;
            }
            out.write_all(b"\n")?;
        }
        Ok(())
    }
}

struct Slot {
    align: Align,
    fill: usize,
    first: bool,
    last: bool,
}

impl Slot {
    fn write(
        self,
        out: &mut dyn Write,
        styled: Styled,
        text: &str,
        tone: Option<Tone>,
    ) -> io::Result<()> {
        let fill = self.fill;
        if !self.first {
            out.write_all(GUTTER.as_bytes())?;
        }
        if self.align == Align::Right {
            write!(out, "{:fill$}", "")?;
        }
        match tone {
            Some(tone) => write!(out, "{}", styled.paint(tone, text))?,
            None => out.write_all(text.as_bytes())?,
        }
        if self.align == Align::Left && !self.last {
            write!(out, "{:fill$}", "")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLUMNS: [Column; 3] = [
        Column::left("Type"),
        Column::right("Created"),
        Column::right("Failed"),
    ];

    // Text is left aligned, numbers right aligned, two-space gutter, no trailing spaces.
    #[test]
    fn plain_table_aligns_columns() {
        let mut table = Table::new(&COLUMNS);
        table
            .cell("Mailboxes", None)
            .count(12, None)
            .count(0, Some(Tone::Bad));
        table
            .cell("Emails", None)
            .count(4210, None)
            .count(2, Some(Tone::Bad));
        let mut out = Vec::new();
        table.write(Styled::Plain, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "  Type       Created  Failed\n  Mailboxes       12       0\n  Emails       4,210       2\n"
        );
    }

    // Escapes wrap the text only, so styled columns keep the plain alignment.
    #[test]
    fn styled_table_keeps_alignment() {
        let mut table = Table::new(&COLUMNS);
        table
            .cell("Emails", None)
            .count(0, None)
            .count(3, Some(Tone::Bad));
        let mut out = Vec::new();
        table.write(Styled::Ansi, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b[2m0\x1b[0m"));
        assert!(text.ends_with("       \x1b[31m3\x1b[0m\n"));
    }
}
