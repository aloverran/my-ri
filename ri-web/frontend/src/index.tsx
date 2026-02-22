/* @refresh reload */
import { render } from 'solid-js/web';
import { initHighlighter } from './highlight';
import App from './App';

const root = document.getElementById('root');

// Initialize syntax highlighter (loads WASM + grammars), then mount the app.
// This ensures highlighted code blocks work from the first render.
initHighlighter().then(() => {
  render(() => <App />, root!);
});
