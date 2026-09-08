import { createRoot } from 'react-dom/client';
import { App } from './App';
import { desktopApi } from './api/desktop';
import './styles.css';

const demo = new URLSearchParams(window.location.search).get('demo') === '1';
async function main() {
  const api = demo ? (await import('./api/demo')).createDemoApi() : desktopApi;
  createRoot(document.getElementById('root')!).render(<App api={api} demo={demo} />);
}
void main();
