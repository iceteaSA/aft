use std::collections::BTreeSet;
use std::io::BufReader;

use aft::bash_permissions::scan::scan_with_project_root;
use brush_parser::ast::{self, Command, CommandPrefixOrSuffixItem, CompoundCommand, IoFileRedirectTarget, IoRedirect};
use brush_parser::word::{self, WordPiece, WordPieceWithSource};
use brush_parser::{Parser, ParserOptions};

mod corpus;

#[derive(Debug, Default)]
struct Observation {
    commands: Vec<ObservedCommand>,
    redirections: Vec<String>,
    nested_expansions: Vec<String>,
    dynamic: Vec<String>,
}

#[derive(Debug)]
struct ObservedCommand {
    name: String,
    name_span: Option<(usize, usize)>,
    args: Vec<String>,
    arg_spans: Vec<Option<(usize, usize)>>,
}

fn main() {
    let project_root = std::env::temp_dir().join("brush-parser-scanner-spike-project");
    let _ = std::fs::create_dir_all(&project_root);
    let options = ParserOptions::default();

    println!("brush-parser-scanner-spike version=0.1.0");
    println!("corpus_entries={}", corpus::CORPUS.len());
    println!("format=CASE\tORIGIN\tPARSE\tBRUSH_COMMANDS\tPIPELINES\tREDIRECTIONS\tAFT_SCANNER\tBUCKET");

    let mut counts = [0usize; 4];
    for entry in corpus::CORPUS {
        let brush = parse_entry(entry.command, &options);
        let scanner = scan_with_project_root(entry.command, &project_root, &project_root);
        let scanner_summary = scanner_summary(&scanner);
        let (bucket, reason) = classify(&brush, &scanner_summary);
        counts[bucket_index(&bucket)] += 1;

        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            entry.id,
            entry.origin,
            brush.parse_status,
            format_commands(&brush.observation),
            format_pipelines(&brush),
            format_redirections(&brush.observation),
            scanner_summary.display,
            format!("{bucket}: {reason}"),
        );
        if !brush.observation.nested_expansions.is_empty() || !brush.observation.dynamic.is_empty() {
            println!(
                "{}\tDETAIL\tnested={}\tdynamic={}",
                entry.id,
                brush.observation.nested_expansions.join(","),
                brush.observation.dynamic.join(",")
            );
        }
    }

    println!("BUCKET_COUNTS\tAGREE={}\tTS-BLIND={}\tBRUSH-GAP={}\tBOTH-LIMITED={}", counts[0], counts[1], counts[2], counts[3]);
}

struct BrushResult {
    parse_status: String,
    observation: Observation,
    pipelines: Vec<String>,
    parse_error: Option<String>,
}

fn parse_entry(source: &str, options: &ParserOptions) -> BrushResult {
    let mut parser = Parser::new(BufReader::new(source.as_bytes()), options);
    match parser.parse_program() {
        Ok(program) => {
            let mut result = BrushResult {
                parse_status: "ok".to_string(),
                observation: Observation::default(),
                pipelines: Vec::new(),
                parse_error: None,
            };
            visit_program(&program, &mut result);
            result
        }
        Err(error) => BrushResult {
            parse_status: "failed".to_string(),
            observation: Observation::default(),
            pipelines: Vec::new(),
            parse_error: Some(error.to_string()),
        },
    }
}

fn visit_program(program: &ast::Program, result: &mut BrushResult) {
    for list in &program.complete_commands {
        visit_list(list, result);
    }
}

fn visit_list(list: &ast::CompoundList, result: &mut BrushResult) {
    for item in &list.0 {
        visit_and_or(&item.0, result);
    }
}

fn visit_and_or(list: &ast::AndOrList, result: &mut BrushResult) {
    let mut structure = vec![pipeline_label(&list.first)];
    for item in &list.additional {
        let (operator, pipeline) = match item {
            ast::AndOr::And(pipeline) => ("&&", pipeline),
            ast::AndOr::Or(pipeline) => ("||", pipeline),
        };
        structure.push(format!("{operator} {}", pipeline_label(pipeline)));
    }
    result.pipelines.push(structure.join(" "));
    visit_pipeline(&list.first, result);
    for item in &list.additional {
        let pipeline = match item {
            ast::AndOr::And(pipeline) | ast::AndOr::Or(pipeline) => pipeline,
        };
        visit_pipeline(pipeline, result);
    }
}

fn visit_pipeline(pipeline: &ast::Pipeline, result: &mut BrushResult) {
    for command in &pipeline.seq {
        visit_command(command, result);
    }
}

fn pipeline_label(pipeline: &ast::Pipeline) -> String {
    let stages = pipeline
        .seq
        .iter()
        .map(command_label)
        .collect::<Vec<_>>()
        .join(" | ");
    let mut label = String::new();
    if pipeline.timed.is_some() {
        label.push_str("time ");
    }
    if pipeline.bang {
        label.push_str("! ");
    }
    label.push_str(&stages);
    label
}

fn command_label(command: &Command) -> String {
    match command {
        Command::Simple(simple) => simple
            .word_or_name
            .as_ref()
            .map_or_else(|| "<no-command>".into(), |word| word.value.clone()),
        Command::Compound(_, _) => "<compound>".into(),
        Command::Function(_) => "<function-definition>".into(),
        Command::ExtendedTest(_, _) => "[[ ]]".into(),
    }
}

fn visit_command(command: &Command, result: &mut BrushResult) {
    match command {
        Command::Simple(simple) => visit_simple(simple, result),
        Command::Compound(compound, redirects) => {
            if let Some(redirects) = redirects {
                visit_redirect_list(redirects, result);
            }
            visit_compound(compound, result);
        }
        // Defining a function does not execute its body. The later invocation
        // is represented as a normal simple command and remains observable.
        Command::Function(_) => result.observation.dynamic.push("function-definition".into()),
        Command::ExtendedTest(_, redirects) => {
            if let Some(redirects) = redirects {
                visit_redirect_list(redirects, result);
            }
        }
    }
}

fn visit_compound(compound: &CompoundCommand, result: &mut BrushResult) {
    match compound {
        CompoundCommand::Arithmetic(_) | CompoundCommand::ArithmeticForClause(_) => {}
        CompoundCommand::BraceGroup(group) => visit_list(&group.list, result),
        CompoundCommand::Subshell(subshell) => visit_list(&subshell.list, result),
        CompoundCommand::ForClause(clause) => visit_list(&clause.body.list, result),
        CompoundCommand::CaseClause(clause) => {
            for case in &clause.cases {
                if let Some(body) = &case.cmd {
                    visit_list(body, result);
                }
            }
        }
        CompoundCommand::IfClause(clause) => {
            visit_list(&clause.condition, result);
            visit_list(&clause.then, result);
            if let Some(elses) = &clause.elses {
                for clause in elses {
                    if let Some(condition) = &clause.condition {
                        visit_list(condition, result);
                    }
                    visit_list(&clause.body, result);
                }
            }
        }
        CompoundCommand::WhileClause(clause) | CompoundCommand::UntilClause(clause) => {
            visit_list(&clause.0, result);
            visit_list(&clause.1.list, result);
        }
        CompoundCommand::Coprocess(coprocess) => visit_command(&coprocess.body, result),
    }
}

fn visit_simple(simple: &ast::SimpleCommand, result: &mut BrushResult) {
    if let Some(prefix) = &simple.prefix {
        visit_items(&prefix.0, result);
    }
    let Some(name) = &simple.word_or_name else {
        if simple.prefix.is_some() || simple.suffix.is_some() {
            result.observation.dynamic.push("no-command-word".into());
        }
        return;
    };

    let name_text = name.value.clone();
    let mut args = Vec::new();
    let mut arg_spans = Vec::new();
    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            match item {
                CommandPrefixOrSuffixItem::Word(word) => {
                    args.push(word.value.clone());
                    arg_spans.push(word.loc.as_ref().map(|span| (span.start.index, span.end.index)));
                }
                CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                    args.push(word.value.clone());
                    arg_spans.push(word.loc.as_ref().map(|span| (span.start.index, span.end.index)));
                }
                CommandPrefixOrSuffixItem::IoRedirect(redirect) => visit_redirect(redirect, result),
                CommandPrefixOrSuffixItem::ProcessSubstitution(kind, subshell) => {
                    result.observation.redirections.push(format!("process-substitution:{kind:?}"));
                    visit_list(&subshell.list, result);
                }
            }
        }
    }
    result.observation.commands.push(ObservedCommand {
        name: name_text.clone(),
        name_span: name.loc.as_ref().map(|span| (span.start.index, span.end.index)),
        args: args.clone(),
        arg_spans,
    });
    if is_dynamic_command_word(name) {
        result.observation.dynamic.push(format!("command-word:{name_text}"));
    }
    if name_text == "eval" {
        result.observation.dynamic.push("eval-chain".into());
    } else if matches!(name_text.as_str(), "source" | ".") {
        result.observation.dynamic.push("source-chain".into());
    }
    if name_text == "bash" || name_text == "sh" || name_text == "command" {
        if args.iter().any(|arg| arg == "-c" || arg == "-O" || arg == "-o") {
            result.observation.dynamic.push(format!("shell-eval:{name_text}"));
        }
    }
    visit_word_expansions(name, result);
    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            if let CommandPrefixOrSuffixItem::Word(word) | CommandPrefixOrSuffixItem::AssignmentWord(_, word) = item {
                visit_word_expansions(word, result);
            }
        }
    }
}

fn visit_items(items: &[CommandPrefixOrSuffixItem], result: &mut BrushResult) {
    for item in items {
        match item {
            CommandPrefixOrSuffixItem::IoRedirect(redirect) => visit_redirect(redirect, result),
            CommandPrefixOrSuffixItem::ProcessSubstitution(kind, subshell) => {
                result.observation.redirections.push(format!("process-substitution:{kind:?}"));
                visit_list(&subshell.list, result);
            }
            CommandPrefixOrSuffixItem::Word(word) => visit_word_expansions(word, result),
            CommandPrefixOrSuffixItem::AssignmentWord(_, word) => visit_word_expansions(word, result),
        }
    }
}

fn visit_redirect_list(redirects: &ast::RedirectList, result: &mut BrushResult) {
    for redirect in &redirects.0 {
        visit_redirect(redirect, result);
    }
}

fn visit_redirect(redirect: &IoRedirect, result: &mut BrushResult) {
    match redirect {
        IoRedirect::File(fd, kind, target) => {
            result.observation.redirections.push(format!("file fd={fd:?} kind={kind:?} target={}", redirect_target(target)));
            match target {
                IoFileRedirectTarget::Filename(word) | IoFileRedirectTarget::Duplicate(word) => {
                    visit_word_expansions(word, result);
                }
                IoFileRedirectTarget::ProcessSubstitution(_, subshell) => {
                    visit_list(&subshell.list, result);
                }
                IoFileRedirectTarget::Fd(_) => {}
            }
        }
        IoRedirect::HereDocument(fd, here) => {
            result.observation.redirections.push(format!("heredoc fd={fd:?} end={:?} body={:?}", here.here_end.value, here.doc.value));
            visit_word_expansions(&here.doc, result);
        }
        IoRedirect::HereString(fd, word) => {
            result.observation.redirections.push(format!("herestring fd={fd:?} value={:?}", word.value));
            visit_word_expansions(word, result);
        }
        IoRedirect::OutputAndError(word, append) => {
            result.observation.redirections.push(format!("output-and-error append={append} target={:?}", word.value));
            visit_word_expansions(word, result);
        }
    }
}

fn redirect_target(target: &IoFileRedirectTarget) -> String {
    match target {
        IoFileRedirectTarget::Filename(word) | IoFileRedirectTarget::Duplicate(word) => word.value.clone(),
        IoFileRedirectTarget::Fd(fd) => fd.to_string(),
        IoFileRedirectTarget::ProcessSubstitution(kind, _) => format!("process-substitution:{kind:?}"),
    }
}

fn visit_word_expansions(word: &ast::Word, result: &mut BrushResult) {
    let Ok(pieces) = word::parse(&word.value, &ParserOptions::default()) else {
        return;
    };
    visit_pieces(&pieces, result);
}

fn visit_pieces(pieces: &[WordPieceWithSource], result: &mut BrushResult) {
    for piece in pieces {
        match &piece.piece {
            WordPiece::CommandSubstitution(text) => {
                result.observation.nested_expansions.push(format!("$({text})"));
                let nested = parse_entry(text, &ParserOptions::default());
                if nested.parse_status == "ok" {
                    merge_observation(result, nested.observation, nested.pipelines);
                } else {
                    result.observation.dynamic.push(format!("nested-parse-failed:{text}"));
                }
            }
            WordPiece::BackquotedCommandSubstitution(text) => {
                result.observation.nested_expansions.push(format!("`{text}`"));
                let nested = parse_entry(text, &ParserOptions::default());
                if nested.parse_status == "ok" {
                    merge_observation(result, nested.observation, nested.pipelines);
                } else {
                    result.observation.dynamic.push(format!("nested-parse-failed:{text}"));
                }
            }
            WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => visit_pieces(inner, result),
            WordPiece::ParameterExpansion(_) => result.observation.dynamic.push("parameter-expansion".into()),
            WordPiece::ArithmeticExpression(_) => result.observation.dynamic.push("arithmetic-expansion".into()),
            _ => {}
        }
    }
}

fn merge_observation(result: &mut BrushResult, nested: Observation, pipelines: Vec<String>) {
    result.observation.commands.extend(nested.commands);
    result.observation.redirections.extend(nested.redirections);
    result.observation.nested_expansions.extend(nested.nested_expansions);
    result.observation.dynamic.extend(nested.dynamic);
    result.pipelines.extend(pipelines);
}

fn is_dynamic_command_word(word: &ast::Word) -> bool {
    word::parse(&word.value, &ParserOptions::default()).map_or(true, |pieces| contains_dynamic_command_piece(&pieces))
}

fn contains_dynamic_command_piece(pieces: &[WordPieceWithSource]) -> bool {
    pieces.iter().any(|piece| match &piece.piece {
        WordPiece::ParameterExpansion(_) | WordPiece::CommandSubstitution(_) | WordPiece::BackquotedCommandSubstitution(_) => true,
        WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => contains_dynamic_command_piece(inner),
        _ => false,
    })
}

#[derive(Debug)]
struct ScannerSummary {
    names: BTreeSet<String>,
    display: String,
    wildcard: bool,
    no_asks: bool,
}

fn scanner_summary(asks: &[aft::bash_permissions::PermissionAsk]) -> ScannerSummary {
    let mut names = BTreeSet::new();
    let mut rendered = Vec::new();
    let mut wildcard = false;
    for ask in asks {
        rendered.push(format!("{ask:?}"));
        if matches!(ask.kind, aft::bash_permissions::PermissionKind::Bash) {
            for pattern in &ask.patterns {
                if pattern == "*" {
                    wildcard = true;
                } else if let Some(name) = first_shell_word(pattern) {
                    names.insert(name);
                }
            }
        }
    }
    ScannerSummary {
        names,
        display: if rendered.is_empty() { "none".into() } else { rendered.join(";") },
        wildcard,
        no_asks: asks.is_empty(),
    }
}

fn first_shell_word(source: &str) -> Option<String> {
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in source.trim().chars() {
        if escaped {
            token.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                token.push(ch);
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if matches!(ch, ' ' | '\t' | '\n' | ';' | '|' | '&' | '>' | '<') {
            break;
        }
        token.push(ch);
    }
    if token.is_empty() || token.ends_with('=') {
        return None;
    }
    if let Some((left, _)) = token.split_once('=') {
        if left.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
    }
    Some(token)
}

fn format_commands(observation: &Observation) -> String {
    if observation.commands.is_empty() {
        return "none".into();
    }
    observation.commands.iter().map(|command| {
        let span = command.name_span.map_or_else(|| "?".into(), |(start, end)| format!("{start}..{end}"));
        let args = command
            .args
            .iter()
            .zip(command.arg_spans.iter())
            .map(|(arg, span)| {
                let span = span.map_or_else(|| "?".into(), |(start, end)| format!("{start}..{end}"));
                format!("{arg}@{span}")
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("{}({})@{}", command.name, args, span)
    }).collect::<Vec<_>>().join(" | ")
}

fn format_pipelines(result: &BrushResult) -> String {
    if result.parse_status == "failed" {
        return format!("parse-error:{}", result.parse_error.as_deref().unwrap_or("unknown"));
    }
    if result.pipelines.is_empty() { "none".into() } else { result.pipelines.join(" ; ") }
}

fn format_redirections(observation: &Observation) -> String {
    if observation.redirections.is_empty() { "none".into() } else { observation.redirections.join(" | ") }
}

fn classify(brush: &BrushResult, scanner: &ScannerSummary) -> (&'static str, String) {
    if brush.parse_status == "failed" {
        if scanner.wildcard {
            return ("AGREE", "both reject malformed input; AFT fails closed with a wildcard".into());
        }
        if scanner.no_asks {
            return ("BOTH-LIMITED", "both produced no executable command verdict".into());
        }
        return ("BRUSH-GAP", "brush failed while AFT identified command(s)".into());
    }
    let mut brush_names: BTreeSet<_> = brush
        .observation
        .commands
        .iter()
        .map(|command| normalize_name(&command.name))
        // AFT intentionally suppresses permission asks for directory-changing
        // builtins; compare the permission-relevant command set here.
        .filter(|name| !matches!(name.as_str(), "cd" | "pushd" | "popd"))
        .collect();
    // AFT adds a second ask for the command xargs will invoke. The brush AST
    // faithfully exposes that word as an xargs argument, but it is not itself
    // a second AST command node.
    for command in &brush.observation.commands {
        if command.name == "xargs" {
            if let Some(delegated) = command.args.iter().find(|arg| !arg.starts_with('-')) {
                brush_names.insert(normalize_name(delegated));
            }
        }
    }
    let scanner_names: BTreeSet<_> = scanner.names.iter().map(|name| normalize_name(name)).collect();
    if brush.observation.dynamic.iter().any(|item| {
        item.contains("eval")
            || item.starts_with("command-word:")
            || item.contains("shell-eval")
            || item.contains("source-chain")
    }) {
        return ("BOTH-LIMITED", format!("dynamic execution or command construction: {}", brush.observation.dynamic.join(", ")));
    }
    if brush_names != scanner_names {
        if brush_names.difference(&scanner_names).next().is_some() {
            return ("TS-BLIND", format!("brush commands not represented by AFT asks: brush={brush_names:?} aft={scanner_names:?}"));
        }
        return ("BRUSH-GAP", format!("AFT identified names absent from brush traversal: brush={brush_names:?} aft={scanner_names:?}"));
    }
    if scanner.wildcard {
        return ("AGREE", "AFT fail-closed wildcard covers the parsed construct".into());
    }
    ("AGREE", "same statically identifiable command names".into())
}

fn normalize_name(name: &str) -> String {
    name.trim_matches(|c| c == '\'' || c == '"').to_string()
}

fn bucket_index(bucket: &str) -> usize {
    match bucket { "AGREE" => 0, "TS-BLIND" => 1, "BRUSH-GAP" => 2, _ => 3 }
}
