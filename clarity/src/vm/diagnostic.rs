// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use crate::vm::representations::Span;
use std::fmt;

/// In a near future, we can go further in our static analysis and provide different levels
/// of diagnostics, such as warnings, hints, best practices, etc.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub enum Level {
    Error,
}

pub trait DiagnosableError {
    fn message(&self) -> String;
    fn suggestion(&self) -> Option<String>;
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
    pub spans: Vec<Span>,
    pub suggestion: Option<String>,
}

impl Diagnostic {
    pub fn err(error: &dyn DiagnosableError) -> Diagnostic {
        Diagnostic {
            spans: vec![],
            level: Level::Error,
            message: error.message(),
            suggestion: error.suggestion(),
        }
    }

    pub fn add_span(&mut self, start_line: u32, start_column: u32, end_line: u32, end_column: u32) {
        self.spans.push(Span {
            start_line,
            start_column,
            end_line,
            end_column,
        });
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.level)?;
        if self.spans.len() == 1 {
            write!(
                f,
                " (line {}, column {})",
                self.spans[0].start_line, self.spans[0].start_column
            )?;
        } else if self.spans.len() > 1 {
            let lines: Vec<String> = self
                .spans
                .iter()
                .map(|s| format!("line: {}", s.start_line))
                .collect();
            write!(f, " ({})", lines.join(", "))?;
        }
        write!(f, ": {}.", &self.message)?;
        if let Some(suggestion) = &self.suggestion {
            write!(f, "\n{}", suggestion)?;
        }
        write!(f, "\n")
    }
}
