use std::path::PathBuf;

use crate::config::Configuration;
use crate::error::Error;
use crate::metadata::compile_codebase_for_sources;
use crate::source;
use ahash::HashMap;
use ahash::HashMapExt;
use mago_codex::metadata::CodebaseMetadata;
use mago_codex::reference::SymbolReferences;
use mago_interner::ThreadedInterner;
use mago_names::resolver::NameResolver;
use mago_reporting::AnnotationKind;
use mago_reporting::Issue;
use mago_reporting::IssueCollection;
use mago_reporting::Level;
use mago_semantics::SemanticsChecker;
use mago_source::Source;
use mago_source::SourceCategory;
use mago_source::SourceManager;
use mago_span::HasPosition;
use mago_span::HasSpan;
use mago_span::Span;
use mago_syntax::ast::Hint;
use mago_syntax::ast::Node;
use mago_syntax::ast::Program;
use mago_syntax::parser::parse_source;
use tower_lsp::lsp_types::*;

#[derive(Debug)]
pub(super) struct MagoWorkspace {
    configuration: Configuration,
    source_manager: SourceManager,
    issues: IssueCollection,
    codebase: CodebaseMetadata,
    references: SymbolReferences,
    program_map: HashMap<Option<PathBuf>, Program>,
}

impl MagoWorkspace {
    pub async fn initialize(interner: &ThreadedInterner, root: PathBuf) -> Result<Self, Error> {
        let configuration = Configuration::from_workspace(root);
        let source_manager = source::load(interner, &configuration.source, true, true).await?;
        let sources: Vec<_> = source_manager.source_ids_for_category(SourceCategory::UserDefined);
        let references = &mut SymbolReferences::new();
        let length = sources.len();

        let mut codebase = compile_codebase_for_sources(&source_manager, references, interner).await?;
        eprintln!("references: {:?}", references);
        let mut handles = Vec::with_capacity(length);

        for source_id in sources {
            handles.push(tokio::spawn({
                let interner = interner.clone();
                let manager = source_manager.clone();

                async move {
                    let source = manager.load(&source_id)?;
                    let name_resolver = NameResolver::new(&interner);
                    let semantics_checker = SemanticsChecker::new(&configuration.php_version, &interner);
                    let (program, parse_error) = parse_source(&interner, &source);
                    let resolved_names = name_resolver.resolve(&program);
                    eprintln!("resolved names: {:?}", resolved_names);

                    let mut semantic_issues = semantics_checker.check(&source, &program, &resolved_names);
                    if let Some(error) = &parse_error {
                        semantic_issues.push(Into::<Issue>::into(error));
                    }

                    Result::<_, Error>::Ok((source.path, program, semantic_issues))
                }
            }));
        }

        let mut issues = Vec::new();
        let mut program_map = HashMap::with_capacity(length);

        for handle in handles {
            let (path, program, issue_collection) = handle.await??;

            issues.extend(issue_collection);
            program_map.insert(path, program);
        }
        issues.extend(codebase.take_issues(true));

        let issues = IssueCollection::from(issues);
        let references = references.clone();

        Ok(MagoWorkspace { configuration, source_manager, issues, codebase, references, program_map })
    }

    pub async fn get_workspace_diagnostic_report(&self) -> Result<WorkspaceDiagnosticReport, Error> {
        let mut hashmap = HashMap::default();
        for issue in self.issues.iter() {
            tracing::error!("issue: {:?}", issue);

            let (uri, diagnostic) = issue_to_diagnostic("semantics", &self.source_manager, issue)?;
            hashmap.entry(uri).or_insert_with(Vec::new).push(diagnostic);
        }

        let mut reports = vec![];
        for (uri, diagnostics) in hashmap {
            let report = WorkspaceFullDocumentDiagnosticReport {
                uri,
                version: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport { result_id: None, items: diagnostics },
            };

            reports.push(WorkspaceDocumentDiagnosticReport::Full(report));
        }

        Ok(WorkspaceDiagnosticReport { items: reports })
    }

    pub async fn get_document_diagnostic(
        &self,
        document_url: &Url,
    ) -> Result<RelatedFullDocumentDiagnosticReport, Error> {
        let diagnostics: Vec<Diagnostic> = self
            .issues
            .iter()
            .filter_map(|issue| {
                let (url, diagnostic) = issue_to_diagnostic("semantics", &self.source_manager, issue).ok()?;

                if url == *document_url { Some(diagnostic) } else { None }
            })
            .collect();

        Ok(RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport { result_id: None, items: diagnostics },
        })
    }

    pub async fn find_references(&self, document_url: &Url, position: &Position) {
        let path = PathBuf::from(document_url.path());
        if let Some(program) = self.program_map.get(&Some(path)) {
            if let Some(node) = get_node_for_position(
                &Node::Program(&program),
                &self.source_manager.load(&program.source).unwrap(),
                position,
            ) {
                match node {
                    Node::FunctionLikeParameter(parameter) => {
                        if let Some(hint) = parameter.hint.clone() {
                            match hint {
                                Hint::Identifier(id) => {
                                    todo!("find references");
                                    //self.references.get_symbol_references_to_symbols_in_signature().get(&id)
                                }
                                _ => todo!(),
                            }
                        }
                    }
                    _ => {
                        todo!()
                    }
                }
            }
        }
    }
}

fn get_node_for_position<'a>(node: &Node<'a>, source: &Source, needle: &Position) -> Option<Node<'a>> {
    let range = get_range(node, source);

    if (range.start.line..=range.end.line).contains(&needle.line)
        && (range.start.character..=range.end.character).contains(&needle.character)
    {
        return Some(*node);
    }

    for node in node.children() {
        if let Some(n) = get_node_for_position(&node, source, needle) {
            return Some(n);
        }
    }

    None
}

fn get_range(node: impl HasSpan, source: &Source) -> Range {
    Range {
        start: Position {
            line: (source.line_number(node.start_position().offset())) as u32,
            character: (source.column_number(node.start_position().offset())) as u32,
        },
        end: Position {
            line: (source.line_number(node.end_position().offset())) as u32,
            character: (source.column_number(node.end_position().offset())) as u32,
        },
    }
}

fn issue_to_diagnostic(
    issue_source: &str,
    source_manager: &SourceManager,
    issue: &Issue,
) -> Result<(Url, Diagnostic), Error> {
    let primary_annotation = issue
        .annotations
        .iter()
        .find(|p| matches!(p.kind, AnnotationKind::Primary))
        .expect("issue should have at least one annotation");

    let span = &primary_annotation.span;
    let location = span_to_location(source_manager, span)?;

    // Convert Level to DiagnosticSeverity
    let severity = match issue.level {
        Level::Note => Some(DiagnosticSeverity::HINT),
        Level::Help => Some(DiagnosticSeverity::INFORMATION),
        Level::Warning => Some(DiagnosticSeverity::WARNING),
        Level::Error => Some(DiagnosticSeverity::ERROR),
    };

    let code: Option<NumberOrString> = issue.code.clone().map(NumberOrString::String);

    let mut related_information = Vec::new();
    for annotation in issue.annotations.iter() {
        let location = span_to_location(source_manager, &annotation.span)?;
        related_information
            .push(DiagnosticRelatedInformation { location, message: annotation.message.clone().unwrap_or_default() });
    }

    let diagnostic = Diagnostic {
        range: location.range,
        severity,
        code,
        code_description: None,
        message: issue.message.clone(),
        related_information: Some(related_information),
        tags: None,
        data: None,
        source: Some(issue_source.to_string()),
    };

    Ok((location.uri, diagnostic))
}

fn span_to_location(source_manager: &SourceManager, span: &Span) -> Result<Location, Error> {
    let source_id = &span.start.source;
    let source = source_manager.load(source_id)?;

    let range = Range {
        start: Position {
            line: (source.line_number(span.start.offset)) as u32,
            character: (source.column_number(span.start.offset)) as u32,
        },
        end: Position {
            line: (source.line_number(span.end.offset)) as u32,
            character: (source.column_number(span.end.offset)) as u32,
        },
    };

    let file_path = PathBuf::from(&source.path.expect("source must have a path"));
    let url = Url::from_file_path(file_path).expect("file path must be valid");

    Ok(Location { uri: url, range })
}
