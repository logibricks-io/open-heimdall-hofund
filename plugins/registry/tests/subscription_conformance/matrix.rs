use hofund_plugin_api::ProtocolId;

use crate::vendors;

pub const EXECUTED_CELLS: usize = 93;
pub const NA_CELLS: usize = 147;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Chat,
    Responses,
    Messages,
    Generative,
}

pub const SHAPES: [Shape; 4] = [
    Shape::Chat,
    Shape::Responses,
    Shape::Messages,
    Shape::Generative,
];

impl Shape {
    pub const fn protocol(self) -> ProtocolId {
        match self {
            Self::Chat => ProtocolId::OPENAI_CHAT,
            Self::Responses => ProtocolId::OPENAI_RESPONSES,
            Self::Messages => ProtocolId::ANTHROPIC_MESSAGES,
            Self::Generative => ProtocolId::GENERATIVE_LANGUAGE,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Chat => "OpenAI Chat",
            Self::Responses => "OpenAI Responses",
            Self::Messages => "Anthropic Messages",
            Self::Generative => "Generative Language",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Support {
    pub unary: bool,
    pub stream: bool,
    pub tools: bool,
}

impl Support {
    pub const fn new(unary: bool, stream: bool) -> Self {
        Self {
            unary,
            stream,
            tools: true,
        }
    }

    const fn cell(self, stream: bool, tools: bool) -> bool {
        (if stream { self.stream } else { self.unary }) && (!tools || self.tools)
    }
}

pub struct Row {
    pub shape: Shape,
    pub model: &'static str,
    pub support: Support,
}

pub struct Vendor {
    pub id: &'static str,
    pub name: &'static str,
    pub rows: &'static [Row],
}

#[derive(Clone, Copy)]
pub struct Cell {
    pub vendor: &'static str,
    pub shape: Shape,
    pub model: &'static str,
    pub stream: bool,
    pub tools: bool,
}

impl Cell {
    pub fn label(self) -> String {
        format!(
            "{} / {} / {} / {}",
            self.vendor,
            self.shape.name(),
            if self.stream { "stream" } else { "unary" },
            if self.tools { "tools" } else { "text" }
        )
    }
}

const fn row(shape: Shape, model: &'static str, unary: bool, stream: bool) -> Row {
    Row {
        shape,
        model,
        support: Support::new(unary, stream),
    }
}

const STREAM_ONLY: bool = false;
const BOTH: bool = true;

static CLAUDE: &[Row] = &[row(Shape::Messages, "claude-sonnet-4-5", STREAM_ONLY, BOTH)];
static CURSOR: &[Row] = &[
    row(Shape::Chat, "auto", STREAM_ONLY, BOTH),
    row(Shape::Messages, "auto", STREAM_ONLY, BOTH),
];
static DEVIN: &[Row] = &[
    row(Shape::Chat, "swe-2", BOTH, BOTH),
    row(Shape::Messages, "swe-2", BOTH, BOTH),
];
static ZED: &[Row] = &[
    row(Shape::Chat, "grok-2", BOTH, BOTH),
    row(Shape::Responses, "gpt-5", BOTH, BOTH),
    row(Shape::Messages, "claude-sonnet-4", BOTH, BOTH),
    row(Shape::Generative, "gemini-2.5-pro", BOTH, BOTH),
];
static CODEX: &[Row] = &[row(Shape::Responses, "gpt-5", BOTH, BOTH)];
static COPILOT: &[Row] = &[
    row(Shape::Chat, "gpt-4.1", BOTH, BOTH),
    row(Shape::Responses, "gpt-5", BOTH, BOTH),
    row(Shape::Messages, "claude-sonnet", BOTH, BOTH),
];
static GEMINI: &[Row] = &[row(Shape::Generative, "gemini-2.5-pro", BOTH, BOTH)];
static ANTIGRAVITY: &[Row] = &[row(Shape::Generative, "gemini-3-flash", BOTH, BOTH)];
static GROK: &[Row] = &[row(Shape::Responses, "grok-4.7", BOTH, BOTH)];
static KIRO: &[Row] = &[row(Shape::Messages, "claude-sonnet-4.5", BOTH, BOTH)];
static FACTORY: &[Row] = &[
    row(Shape::Chat, "glm-5.3", BOTH, BOTH),
    row(Shape::Responses, "gpt-5.5", BOTH, BOTH),
    row(Shape::Messages, "claude-sonnet-5-5", BOTH, BOTH),
    row(Shape::Generative, "gemini-4-argon", BOTH, BOTH),
];
static CLINE: &[Row] = &[row(Shape::Chat, "anthropic/claude-sonnet-5.5", BOTH, BOTH)];
static QODER: &[Row] = &[row(Shape::Chat, "qfmodel", BOTH, BOTH)];
static WORKBUDDY: &[Row] = &[row(Shape::Chat, "deepseek-v3", BOTH, BOTH)];
static ZCODE: &[Row] = &[row(Shape::Messages, "glm-5.3", BOTH, BOTH)];

pub const VENDORS: &[Vendor] = &[
    Vendor {
        id: "claude",
        name: "Claude",
        rows: CLAUDE,
    },
    Vendor {
        id: "cursor",
        name: "Cursor",
        rows: CURSOR,
    },
    Vendor {
        id: "devin",
        name: "Devin",
        rows: DEVIN,
    },
    Vendor {
        id: "zed",
        name: "Zed",
        rows: ZED,
    },
    Vendor {
        id: "codex",
        name: "Codex",
        rows: CODEX,
    },
    Vendor {
        id: "copilot",
        name: "Copilot",
        rows: COPILOT,
    },
    Vendor {
        id: "gemini",
        name: "Gemini",
        rows: GEMINI,
    },
    Vendor {
        id: "antigravity",
        name: "Antigravity",
        rows: ANTIGRAVITY,
    },
    Vendor {
        id: "grok",
        name: "Grok",
        rows: GROK,
    },
    Vendor {
        id: "kiro",
        name: "Kiro",
        rows: KIRO,
    },
    Vendor {
        id: "factory",
        name: "Factory",
        rows: FACTORY,
    },
    Vendor {
        id: "cline",
        name: "Cline",
        rows: CLINE,
    },
    Vendor {
        id: "qoder",
        name: "Qoder",
        rows: QODER,
    },
    Vendor {
        id: "workbuddy",
        name: "WorkBuddy",
        rows: WORKBUDDY,
    },
    Vendor {
        id: "zcode",
        name: "Z Code",
        rows: ZCODE,
    },
];

fn cell_supported(vendor: &str, row: Option<&Row>, stream: bool, tools: bool) -> bool {
    let Some(row) = row else {
        return false;
    };
    if !row.support.cell(stream, tools) {
        return false;
    }
    if vendor == "zed" && row.shape == Shape::Chat && !stream && tools {
        return false;
    }
    true
}

pub fn cells() -> Vec<(Cell, bool)> {
    let mut cells = Vec::new();
    for vendor in VENDORS {
        for shape in SHAPES {
            let row = vendor.rows.iter().find(|row| row.shape == shape);
            for stream in [false, true] {
                for tools in [false, true] {
                    let supported = cell_supported(vendor.id, row, stream, tools);
                    cells.push((
                        Cell {
                            vendor: vendor.id,
                            shape,
                            model: row.map_or("", |row| row.model),
                            stream,
                            tools,
                        },
                        supported,
                    ));
                }
            }
        }
    }
    cells
}

pub async fn execute(cell: Cell) -> Result<(), String> {
    vendors::execute(cell).await
}

pub fn render() -> String {
    let mut out = String::from(
        "| Vendor | Inbound shape | Unary text | Unary tools | Stream text | Stream tools |\n\
         |---|---|---|---|---|---|\n",
    );
    for vendor in VENDORS {
        for shape in SHAPES {
            let row = vendor.rows.iter().find(|row| row.shape == shape);
            out.push_str(&format!("| {} | {} |", vendor.name, shape.name()));
            for stream in [false, true] {
                for tools in [false, true] {
                    let supported = cell_supported(vendor.id, row, stream, tools);
                    out.push_str(if supported { " pass |" } else { " N/A |" });
                }
            }
            out.push('\n');
        }
    }
    out
}
