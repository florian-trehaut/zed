use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use editor::{Addon, Editor, EditorEvent};
use gpui::{App, BorrowAppContext, Context, Entity, Global, SharedString, Task, WeakEntity};
use language::{BufferEvent, BufferId, LanguageName, LanguageRegistry, PLAIN_TEXT};
use project::debounced_delay::DebouncedDelay;
use project::{DisableAiSettings, Project};
use std::borrow::Cow;
use std::sync::Arc;
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

const DETECTION_DEBOUNCE: Duration = Duration::from_millis(500);
const MIN_CONTENT_LENGTH: usize = 16;

/// Global language detector state.
/// Tracks which buffers had their language manually overridden by the user,
/// and which buffer is currently being updated by our detection logic.
pub struct LanguageDetector {
    user_overridden_buffers: Mutex<HashSet<BufferId>>,
    currently_applying: Mutex<Option<BufferId>>,
}

impl Global for LanguageDetector {}

impl LanguageDetector {
    fn new() -> Self {
        Self {
            user_overridden_buffers: Mutex::new(HashSet::new()),
            currently_applying: Mutex::new(None),
        }
    }

    fn is_user_overridden(&self, buffer_id: BufferId) -> bool {
        self.user_overridden_buffers
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .contains(&buffer_id)
    }

    fn mark_user_overridden(&self, buffer_id: BufferId) {
        self.user_overridden_buffers
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(buffer_id);
    }

    fn clear_user_override(&self, buffer_id: BufferId) {
        self.user_overridden_buffers
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&buffer_id);
    }

    fn set_currently_applying(&self, buffer_id: Option<BufferId>) {
        *self
            .currently_applying
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = buffer_id;
    }

    fn is_currently_applying(&self, buffer_id: BufferId) -> bool {
        *self
            .currently_applying
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            == Some(buffer_id)
    }
}

/// Per-editor addon holding debounce state for language detection.
struct LanguageDetectionAddon {
    debounce: DebouncedDelay<Editor>,
}

impl Addon for LanguageDetectionAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// Thread-safe Magika session, lazily initialised on first use.
fn magika_session() -> &'static Mutex<magika::Session> {
    static SESSION: OnceLock<Mutex<magika::Session>> = OnceLock::new();
    SESSION.get_or_init(|| {
        Mutex::new(magika::Session::new().expect("failed to initialise Magika session"))
    })
}

pub fn init(cx: &mut App) {
    cx.set_global(LanguageDetector::new());

    cx.observe_new(|editor: &mut Editor, _window, cx: &mut Context<Editor>| {
        if !editor.mode().is_full() {
            return;
        }

        editor.register_addon(LanguageDetectionAddon {
            debounce: DebouncedDelay::new(),
        });

        // Subscribe to editor events (paste / edit).
        cx.subscribe(&cx.entity(), on_editor_event).detach();

        // Subscribe to LanguageChanged on the singleton buffer to detect
        // user-initiated language changes (via language selector).
        let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
            return;
        };
        let buffer_id = buffer.read(cx).remote_id();

        cx.subscribe(&buffer, move |_editor, _buffer, event: &BufferEvent, cx| {
            if !matches!(event, BufferEvent::LanguageChanged(_)) {
                return;
            }
            let detector = cx.global::<LanguageDetector>();
            if !detector.is_currently_applying(buffer_id) {
                detector.mark_user_overridden(buffer_id);
            }
        })
        .detach();

        // Clean up when editor is released.
        cx.on_release(move |_, cx| {
            cx.update_global::<LanguageDetector, _>(|detector: &mut LanguageDetector, _| {
                detector.clear_user_override(buffer_id);
            });
        })
        .detach();

        // Handle stdin: if the buffer already has content when the editor
        // is created, trigger detection immediately.
        let (is_plain_text, has_content) = {
            let snapshot = buffer.read(cx);
            let plain = snapshot
                .language()
                .map_or(true, |lang| lang.name().as_ref() == "Plain Text");
            let content = snapshot.len() >= MIN_CONTENT_LENGTH;
            (plain, content)
        };

        if is_plain_text && has_content {
            trigger_detection(editor, &buffer, buffer_id, cx);
        }
    })
    .detach();
}

fn on_editor_event(
    editor: &mut Editor,
    _: Entity<Editor>,
    event: &EditorEvent,
    cx: &mut Context<Editor>,
) {
    let EditorEvent::Edited { .. } = event else {
        return;
    };

    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };

    let (is_plain_text, buffer_id, content_length) = {
        let snapshot = buffer.read(cx);
        let plain = snapshot
            .language()
            .map_or(true, |lang| lang.name().as_ref() == "Plain Text");
        (plain, snapshot.remote_id(), snapshot.len())
    };

    if !is_plain_text {
        return;
    }

    if DisableAiSettings::is_ai_disabled_for_buffer(Some(&buffer), cx) {
        return;
    }

    if cx
        .global::<LanguageDetector>()
        .is_user_overridden(buffer_id)
    {
        return;
    }

    if content_length < MIN_CONTENT_LENGTH {
        return;
    }

    trigger_detection(editor, &buffer, buffer_id, cx);
}

fn trigger_detection(
    editor: &mut Editor,
    buffer: &Entity<language::Buffer>,
    buffer_id: BufferId,
    cx: &mut Context<Editor>,
) {
    let content: Vec<u8> = buffer.read(cx).text().into_bytes();
    let project = editor.project().cloned();
    let workspace = editor.workspace().map(|ws| ws.downgrade());
    let buffer_handle = buffer.downgrade();

    let Some(addon) = editor.addon_mut::<LanguageDetectionAddon>() else {
        return;
    };

    addon
        .debounce
        .fire_new(DETECTION_DEBOUNCE, cx, move |_editor, cx| {
            let Some(project) = project else {
                return Task::ready(());
            };
            let languages = project.read(cx).languages().clone();
            let languages_for_resolve = languages.clone();

            cx.spawn(async move |_this, cx| {
                let detected = cx
                    .background_executor()
                    .spawn(async move { detect_language(&content, &languages) })
                    .await;

                let Some((language_name, display_name)) = detected else {
                    return;
                };

                let Ok(language) = languages_for_resolve
                    .language_for_name_or_extension(&language_name.0)
                    .await
                else {
                    return;
                };

                cx.update(|cx| {
                    let Some(buffer) = buffer_handle.upgrade() else {
                        return;
                    };

                    cx.update_global::<LanguageDetector, _>(
                        |detector: &mut LanguageDetector, _| {
                            detector.set_currently_applying(Some(buffer_id));
                        },
                    );

                    project.update(cx, |project, cx| {
                        project.set_language_for_buffer(&buffer, language.clone(), cx);
                    });

                    // Defer clearing so the LanguageChanged event still
                    // sees currently_applying when it dispatches.
                    cx.defer(|cx| {
                        cx.update_global::<LanguageDetector, _>(
                            |detector: &mut LanguageDetector, _| {
                                detector.set_currently_applying(None);
                            },
                        );
                    });

                    if let Some(workspace) = workspace.as_ref().and_then(|ws| ws.upgrade()) {
                        show_detection_toast(
                            &workspace,
                            &display_name,
                            buffer.downgrade(),
                            buffer_id,
                            project.downgrade(),
                            cx,
                        );
                    }
                });
            })
        });
}

fn detect_language(
    content: &[u8],
    registry: &Arc<LanguageRegistry>,
) -> Option<(LanguageName, String)> {
    let mut session = magika_session()
        .lock()
        .unwrap_or_else(|err| err.into_inner());

    let result = session.identify_content_sync(content).ok()?;
    let label = result.info().label;
    let extension = magika_label_to_extension(label)?;

    let language_name = registry.language_name_for_extension(extension)?;

    let display_name = language_name.0.to_string();
    Some((language_name, display_name))
}

fn show_detection_toast(
    workspace: &Entity<Workspace>,
    language_display_name: &str,
    buffer: WeakEntity<language::Buffer>,
    buffer_id: BufferId,
    project: WeakEntity<Project>,
    cx: &mut App,
) {
    struct LanguageDetectionNotification;

    static TOAST_COUNTER: AtomicU64 = AtomicU64::new(0);
    let toast_seq = TOAST_COUNTER.fetch_add(1, Ordering::Relaxed);

    let notification_id = NotificationId::composite::<LanguageDetectionNotification>(
        SharedString::from(format!("lang-detect-{}-{}", buffer_id, toast_seq)),
    );

    let message: Cow<'static, str> = format!("Detected: {}", language_display_name).into();

    let toast = Toast::new(notification_id, message)
        .on_click("Undo", move |_window, cx| {
            let Some(buffer) = buffer.upgrade() else {
                return;
            };
            let Some(project) = project.upgrade() else {
                return;
            };

            cx.update_global::<LanguageDetector, _>(|detector: &mut LanguageDetector, _| {
                detector.set_currently_applying(Some(buffer_id));
                detector.clear_user_override(buffer_id);
            });

            project.update(cx, |project, cx| {
                project.set_language_for_buffer(&buffer, PLAIN_TEXT.clone(), cx);
            });

            // Defer clearing currently_applying so the LanguageChanged
            // event (dispatched after this update returns) still sees it.
            cx.defer(|cx| {
                cx.update_global::<LanguageDetector, _>(|detector: &mut LanguageDetector, _| {
                    detector.set_currently_applying(None);
                });
            });
        })
        .autohide();

    workspace.update(cx, |workspace, cx| {
        workspace.show_toast(toast, cx);
    });
}

/// Maps a Magika content type label (from `TypeInfo.label`) to a file
/// extension that the LanguageRegistry can resolve.
/// Returns None for non-code types, ambiguous types, or unknown labels.
fn magika_label_to_extension(label: &str) -> Option<&'static str> {
    match label {
        // Manual overrides: extensionless types and disambiguation cases
        "dockerfile" => Some("Dockerfile"),
        "makefile" => Some("Makefile"),
        "objectivec" => Some("m"),
        "perl" => Some("pl"),
        // Standard code types — extension from Magika's TypeInfo
        "aidl" => Some("aidl"),
        "asm" => Some("asm"),
        "asp" => Some("asp"),
        "autohotkey" => Some("ahk"),
        "autoit" => Some("au3"),
        "awk" => Some("awk"),
        "batch" => Some("bat"),
        "bazel" => Some("bzl"),
        "c" => Some("c"),
        "clojure" => Some("clj"),
        "cmake" => Some("cmake"),
        "cobol" => Some("cob"),
        "coffeescript" => Some("coffee"),
        "cpp" => Some("cpp"),
        "cs" => Some("cs"),
        "css" => Some("css"),
        "dart" => Some("dart"),
        "diff" => Some("diff"),
        "dm" => Some("dm"),
        "elixir" => Some("ex"),
        "erb" => Some("erb"),
        "erlang" => Some("erl"),
        "fortran" => Some("f90"),
        "gemfile" => Some("Gemfile"),
        "go" => Some("go"),
        "gradle" => Some("gradle"),
        "groovy" => Some("groovy"),
        "handlebars" => Some("hbs"),
        "haskell" => Some("hs"),
        "hcl" => Some("hcl"),
        "html" => Some("html"),
        "ini" => Some("ini"),
        "java" => Some("java"),
        "javascript" => Some("js"),
        "jinja" => Some("j2"),
        "json" => Some("json"),
        "jsonl" => Some("jsonl"),
        "julia" => Some("jl"),
        "kotlin" => Some("kt"),
        "latex" => Some("tex"),
        "lisp" => Some("lisp"),
        "lua" => Some("lua"),
        "m4" => Some("m4"),
        "markdown" => Some("md"),
        "matlab" => Some("matlab"),
        "ocaml" => Some("ml"),
        "pascal" => Some("pas"),
        "php" => Some("php"),
        "powershell" => Some("ps1"),
        "prolog" => Some("pro"),
        "proto" => Some("proto"),
        "python" => Some("py"),
        "r" => Some("r"),
        "rst" => Some("rst"),
        "ruby" => Some("rb"),
        "rust" => Some("rs"),
        "scala" => Some("scala"),
        "scss" => Some("scss"),
        "shell" => Some("sh"),
        "smali" => Some("smali"),
        "solidity" => Some("sol"),
        "sql" => Some("sql"),
        "swift" => Some("swift"),
        "tcl" => Some("tcl"),
        "toml" => Some("toml"),
        "typescript" => Some("ts"),
        "vba" => Some("vba"),
        "verilog" => Some("v"),
        "vhdl" => Some("vhd"),
        "vue" => Some("vue"),
        "xml" => Some("xml"),
        "yaml" => Some("yaml"),
        "zig" => Some("zig"),
        // Non-code types and anything unrecognised
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure guard logic: should we attempt language detection on this buffer?
    fn should_detect(language_name: &str, disable_ai: bool, user_overridden: bool) -> bool {
        language_name == "Plain Text" && !disable_ai && !user_overridden
    }

    // ── magika_label_to_extension tests ──────────────────────────

    #[test]
    fn test_maps_python() {
        assert_eq!(magika_label_to_extension("python"), Some("py"));
    }

    #[test]
    fn test_maps_rust() {
        assert_eq!(magika_label_to_extension("rust"), Some("rs"));
    }

    #[test]
    fn test_maps_javascript() {
        assert_eq!(magika_label_to_extension("javascript"), Some("js"));
    }

    #[test]
    fn test_maps_typescript() {
        assert_eq!(magika_label_to_extension("typescript"), Some("ts"));
    }

    #[test]
    fn test_maps_go() {
        assert_eq!(magika_label_to_extension("go"), Some("go"));
    }

    #[test]
    fn test_maps_ruby() {
        assert_eq!(magika_label_to_extension("ruby"), Some("rb"));
    }

    #[test]
    fn test_maps_shell() {
        assert_eq!(magika_label_to_extension("shell"), Some("sh"));
    }

    #[test]
    fn test_maps_dockerfile() {
        assert_eq!(magika_label_to_extension("dockerfile"), Some("Dockerfile"));
    }

    #[test]
    fn test_maps_makefile() {
        assert_eq!(magika_label_to_extension("makefile"), Some("Makefile"));
    }

    #[test]
    fn test_maps_objectivec_not_matlab() {
        assert_eq!(magika_label_to_extension("objectivec"), Some("m"));
    }

    #[test]
    fn test_maps_perl_not_prolog() {
        assert_eq!(magika_label_to_extension("perl"), Some("pl"));
    }

    #[test]
    fn test_rejects_txt() {
        assert_eq!(magika_label_to_extension("txt"), None);
    }

    #[test]
    fn test_rejects_unknown() {
        assert_eq!(magika_label_to_extension("unknown"), None);
    }

    #[test]
    fn test_rejects_empty() {
        assert_eq!(magika_label_to_extension("empty"), None);
    }

    #[test]
    fn test_rejects_csv() {
        assert_eq!(magika_label_to_extension("csv"), None);
    }

    // ── should_detect tests ──────────────────────────────────────

    #[test]
    fn test_should_detect_plain_text_ai_enabled() {
        assert!(should_detect("Plain Text", false, false));
    }

    #[test]
    fn test_should_not_detect_non_plain_text() {
        assert!(!should_detect("Rust", false, false));
    }

    #[test]
    fn test_should_not_detect_when_ai_disabled() {
        assert!(!should_detect("Plain Text", true, false));
    }

    #[test]
    fn test_should_not_detect_when_user_overridden() {
        assert!(!should_detect("Plain Text", false, true));
    }

    #[test]
    fn test_should_not_detect_all_flags_set() {
        assert!(!should_detect("Rust", true, true));
    }
}
