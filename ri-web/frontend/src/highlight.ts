// Syntax highlighting via shiki.
//
// Initializes a highlighter once at startup with a curated set of grammars,
// then wires it into marked's code block renderer. After initHighlighter()
// resolves, every marked() call produces highlighted code blocks.

import { createHighlighter, type Highlighter, type BundledLanguage } from 'shiki';
import { marked } from 'marked';

// Languages we bundle. These cover the vast majority of what an AI coding
// agent will produce. Anything outside this list falls back to unstyled <pre>.
const LANGUAGES: BundledLanguage[] = [
  'rust',
  'typescript',
  'javascript',
  'tsx',
  'jsx',
  'css',
  'html',
  'json',
  'toml',
  'yaml',
  'bash',
  'shell',
  'c',
  'cpp',
  'csharp',
  'glsl',
  'python',
  'markdown',
  'diff',
  'sql',
  'xml',
  'lua',
  'zig',
  'go',
  'swift',
  'hlsl',
  'wgsl',
];

const THEME = 'github-dark-default';

let highlighter: Highlighter | null = null;

export async function initHighlighter(): Promise<void> {
  highlighter = await createHighlighter({
    themes: [THEME],
    langs: LANGUAGES,
  });

  // Wire shiki into marked's code block renderer globally.
  // After this, every marked() call produces highlighted code blocks.
  marked.use({
    renderer: {
      code(code: string, infostring: string | undefined) {
        const lang = (infostring || '').match(/^\S*/)?.[0] || '';
        return highlight(code, lang);
      },
    },
  });
}

// Inline highlight for use in single-line contexts (e.g. tool preview lines).
// Returns just the colored <span> elements without any <pre>/<code> wrapper.
// Falls back to plain escaped text if the language is unknown.
export function highlightInline(code: string, lang: string): string {
  if (!highlighter) return escapeHtml(code);

  const normalized = normalizeLang(lang);
  const loaded = highlighter.getLoadedLanguages();
  if (!normalized || !loaded.includes(normalized)) {
    return escapeHtml(code);
  }

  const html = highlighter.codeToHtml(code, { lang: normalized, theme: THEME });
  // Strip the <pre ...><code> wrapper and </code></pre>, keep the inner spans.
  const inner = html.replace(/^<pre[^>]*><code>/, '').replace(/<\/code><\/pre>$/, '');
  // Also strip the outer <span class="line"> wrapper since this is inline.
  return inner.replace(/^<span class="line">/, '').replace(/<\/span>$/, '');
}

// Synchronous highlight. Returns raw HTML string with <pre> wrapper.
// Falls back to plain <pre><code> if the language is unknown or not loaded.
export function highlight(code: string, lang: string): string {
  const plain = `<pre><code>${escapeHtml(code)}</code></pre>`;
  if (!highlighter) return plain;

  const normalized = normalizeLang(lang);
  const loaded = highlighter.getLoadedLanguages();
  if (!normalized || !loaded.includes(normalized)) {
    return plain;
  }

  return highlighter.codeToHtml(code, {
    lang: normalized,
    theme: THEME,
  });
}

// Single source of truth: extension/alias -> canonical shiki language name.
// Used by both normalizeLang (markdown code fences) and langFromPath (file results).
const LANG_ALIASES: Record<string, string> = {
  'rs': 'rust',
  'ts': 'typescript',
  'tsx': 'tsx',
  'js': 'javascript',
  'jsx': 'jsx',
  'mjs': 'javascript',
  'cjs': 'javascript',
  'mts': 'typescript',
  'cts': 'typescript',
  'css': 'css',
  'html': 'html',
  'htm': 'html',
  'json': 'json',
  'jsonc': 'json',
  'toml': 'toml',
  'yaml': 'yaml',
  'yml': 'yaml',
  'sh': 'bash',
  'bash': 'bash',
  'zsh': 'bash',
  'fish': 'bash',
  'shell': 'shell',
  'c': 'c',
  'h': 'c',
  'cpp': 'cpp',
  'cc': 'cpp',
  'cxx': 'cpp',
  'hpp': 'cpp',
  'hh': 'cpp',
  'c++': 'cpp',
  'cs': 'csharp',
  'glsl': 'glsl',
  'vert': 'glsl',
  'frag': 'glsl',
  'comp': 'glsl',
  'py': 'python',
  'md': 'markdown',
  'diff': 'diff',
  'patch': 'diff',
  'sql': 'sql',
  'xml': 'xml',
  'svg': 'xml',
  'lua': 'lua',
  'zig': 'zig',
  'go': 'go',
  'swift': 'swift',
  'hlsl': 'hlsl',
  'wgsl': 'wgsl',
};

function normalizeLang(lang: string): string {
  const l = lang.toLowerCase().trim();
  return LANG_ALIASES[l] || l;
}

// Infer a shiki language from a file path's extension.
// Returns '' for unknown extensions (caller falls back to plain text).
export function langFromPath(path: string): string {
  const ext = path.split('.').pop()?.toLowerCase() || '';
  return LANG_ALIASES[ext] || '';
}

function escapeHtml(s: string): string {
  return s.replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}
