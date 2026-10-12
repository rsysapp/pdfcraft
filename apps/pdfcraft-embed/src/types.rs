//! The API's TypeScript types, emitted into the generated `pdfcraft_embed.d.ts` (declarations
//! only; no JavaScript is written by hand).

use wasm_bindgen::prelude::wasm_bindgen;

#[wasm_bindgen(typescript_custom_section)]
const TYPES: &str = r#"
export interface MountOptions {
    mode: 'view';
    theme: Theme;
}

export interface Theme {
    dark: boolean;
    accent: string;
    accentContent: string;
}

export type JumpTarget =
    | { kind: 'page'; page: number }
    | { kind: 'text'; text: string; page: number | null; occurrence: number };

export interface Mark {
    id: string;
    text: string;
    page: number | null;
    color: string;
}

export interface EditorAuthor {
    id: string;
    name: string;
}

export interface EditorPermissions {
    acceptRejectChanges: boolean;
    organizePages: boolean;
    fillForms: boolean;
    redact: boolean;
    stamps: boolean;
    drawing: boolean;
}

export type ModeRequest =
    | { mode: 'view' }
    | { mode: 'edit'; author: EditorAuthor; revisionDate: string; permissions: EditorPermissions };

export type EditorEvent =
    | { type: 'opened'; pages: number }
    | { type: 'openFailed'; message: string }
    | { type: 'pageChanged'; page: number }
    | { type: 'selection'; text: string; copyText: string; page: number | null; x: number; y: number }
    | { type: 'selectionCleared' }
    | { type: 'markActivated'; id: string }
    | { type: 'jumpNotFound'; target: JumpTarget }
    | { type: 'modeChanged'; mode: 'view' | 'edit' }
    | { type: 'dirtyChanged'; dirty: boolean }
    | { type: 'saveRequested'; bytes: Uint8Array; fileName: string }
    | { type: 'saveFailed'; message: string }
    | { type: 'commentRequested'; text: string; copyText: string; page: number | null; x: number; y: number }
    | { type: 'redactionsPending'; marks: number }
    | { type: 'editRefused'; code: EditRefusalCode };

export type EditRefusalCode =
    | 'command_not_allowed'
    | 'decision_not_allowed'
    | 'paragraph_break_not_supported'
    | 'character_not_allowed'
    | 'outside_editable_story'
    | 'invalid_edit_session'
    | 'unknown_command'
    | 'invalid_parameters'
    | 'not_available'
    | 'document_protected'
    | 'command_failed';
"#;
