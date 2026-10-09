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

export type EditorEvent =
    | { type: 'opened'; pages: number }
    | { type: 'openFailed'; message: string }
    | { type: 'pageChanged'; page: number }
    | { type: 'selection'; text: string; copyText: string; page: number | null; x: number; y: number }
    | { type: 'selectionCleared' }
    | { type: 'markActivated'; id: string }
    | { type: 'jumpNotFound'; target: JumpTarget };
"#;
