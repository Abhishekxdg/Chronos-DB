import { createRoot } from 'react-dom/client';
import { App } from './App';
import '@fontsource-variable/inter-tight/wght.css';
import '@fontsource-variable/jetbrains-mono/wght.css';
import '@fontsource-variable/doto/wght.css';
import './styles.css';

createRoot(document.getElementById('root')!).render(<App />);
