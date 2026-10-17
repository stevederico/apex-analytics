import './styles.css';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import ErrorBoundary from '@stevederico/skateboard-ui/ErrorBoundary';
import App from './components/App';

const el = document.getElementById('root');
if (!el) throw new Error('#root missing');
el.style.opacity = '1';
createRoot(el).render(
  <StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </StrictMode>,
);
