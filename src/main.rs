mod app;
mod console;
mod designer;
mod diagram;
mod export;
mod lint;
mod schema;
mod sqlparse;

use anyhow::{bail, Context, Result};
use ratatui::crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
};
use std::io::stdout;
use std::path::Path;

const HELP: &str = "\
tuiER — interactive ER diagram of a SQLite database in your terminal

USAGE:
    tuiER <database> [OPTIONS]

OPTIONS:
    --desc-table <NAME>   Table holding descriptions (default: _descriptions)
    --desc-width <N>      Width of the shortened description column (default: 24)
    --select <T[.C]>      Start with a table or column selected
    --read-only           Disable editing descriptions and saving the layout
    --no-pointer-shape    Don't change the mouse pointer (OSC 22)
    --dump                Print the diagram as plain text and exit
    --export <FORMAT>     Print mermaid | dot | md | sql to stdout and exit
    -h, --help            Show this help

DESCRIPTIONS:
    SQLite has no COMMENT ON, so descriptions are stored in a table
    (created automatically on the first edit):
        CREATE TABLE _descriptions (
            table_name  TEXT NOT NULL,
            column_name TEXT,          -- NULL = description of the table
            description TEXT NOT NULL
        );

    Manually arranged table positions are stored in _erd_layout,
    table colours in _erd_colors.

KEYS:
    Press ? inside the viewer for the full list. The essentials:
    arrow keys     move the selection        m    table menu (also right-click)
    click / tab    select                    e    edit (table: whole table,
    /              search                         column: description)
    f / b          follow relation / back    t    new table
    z              focus mode                del  delete table
    space          collapse table            :    query console (read-only)
    shift+arrows   move table on the grid    !    lint
    drag a title   move table (esc cancels)  p/i  preview data / schema
    w a s d, h j k l, drag, wheel  pan       x    export (incl. SQL script)
    u              undo layout (shift+u redo) q    quit

    The view reloads by itself when another program changes the database.
";

struct Args {
    db: String,
    desc_table: String,
    desc_width: usize,
    select: Option<String>,
    dump: bool,
    read_only: bool,
    pointer_shapes: bool,
    export: Option<export::Format>,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    let mut args = Args {
        db: String::new(),
        desc_table: "_descriptions".into(),
        desc_width: 24,
        select: None,
        dump: false,
        read_only: false,
        pointer_shapes: true,
        export: None,
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--desc-table" => args.desc_table = it.next().context("--desc-table needs a value")?,
            "--desc-width" => {
                args.desc_width = it
                    .next()
                    .context("--desc-width needs a value")?
                    .parse()
                    .context("--desc-width must be a number")?
            }
            "--select" => args.select = Some(it.next().context("--select needs a value")?),
            "--dump" => args.dump = true,
            "--read-only" => args.read_only = true,
            "--no-pointer-shape" => args.pointer_shapes = false,
            "--export" => {
                let f = it.next().context("--export needs a format: mermaid, dot or md")?;
                args.export = Some(export::Format::parse(&f).with_context(|| format!("unknown export format '{f}' (mermaid, dot, md)"))?);
            }
            s if s.starts_with('-') => bail!("unknown option '{s}' (see --help)"),
            _ if args.db.is_empty() => args.db = a,
            _ => bail!("unexpected argument '{a}'"),
        }
    }
    if args.db.is_empty() {
        bail!("usage: tuiER <database> [OPTIONS]   (see --help)");
    }
    if !Path::new(&args.db).is_file() {
        bail!("'{}' is not a file", args.db);
    }
    Ok(args)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let schema = schema::load(&args.db, &args.desc_table)?;
    if schema.tables.is_empty() {
        bail!("no tables found in '{}'", args.db);
    }
    let name = Path::new(&args.db)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| args.db.clone());

    if let Some(f) = args.export {
        let layout: Vec<(String, usize, usize)> = schema::load_layout(&args.db)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(n, (c, r))| schema.find_table(&n).map(|t| (schema.tables[t].name.clone(), c, r)))
            .collect();
        let mut layout = layout;
        layout.sort();
        let mut colors: Vec<(String, String)> = schema::load_colors(&args.db)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(n, c)| schema.find_table(&n).map(|t| (schema.tables[t].name.clone(), c)))
            .collect();
        colors.sort();
        let ctx = export::ExportCtx { title: &name, desc_width: args.desc_width, desc_table: &args.desc_table, layout: &layout, colors: &colors };
        print!("{}", f.render(&schema, &ctx));
        return Ok(());
    }

    let mut app = app::App::new(
        schema,
        app::Config {
            db_path: args.db.clone(),
            db_name: name,
            desc_table: args.desc_table.clone(),
            desc_width: args.desc_width,
            read_only: args.read_only,
            pointer_shapes: args.pointer_shapes,
        },
    );
    if let Some(spec) = &args.select {
        let sel = app.resolve(spec)?;
        app.select(Some(sel));
    }

    if args.dump {
        for w in app.warnings() {
            eprintln!("warning: {w}");
        }
        print!("{}", app.canvas_text());
        return Ok(());
    }

    let mut terminal = ratatui::init();
    // make sure mouse capture is released even on panic
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        hook(info);
    }));
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let result = app.run(&mut terminal);
    execute!(stdout(), DisableMouseCapture, DisableBracketedPaste)?;
    ratatui::restore();
    result
}
