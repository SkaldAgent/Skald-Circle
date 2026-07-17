import { AppTopbar }          from './components/topbar.js';
import { AppSidebar }         from './components/sidebar.js';
import { AppCopilot }         from './components/copilot.js';
import { LlmProvidersPage }   from './components/llm-providers.js';
import { ModelsHubPage }          from './components/models-hub.js';
import { ModelsLlmSection }       from './components/models-llm.js';
import { ModelsTranscribeSection } from './components/models-transcribe.js';
import { ModelsImageSection }     from './components/models-image.js';
import { ModelsTtsSection }       from './components/models-tts.js';
import { TasksPage }         from './components/tasks/index.js';
import { AgentsPage }         from './components/agents.js';
import { UsersPage }          from './components/users-page.js';
import { RolesPage }          from './components/roles-page.js';
import { ConnectorsPage }     from './components/connectors.js';
import { ConnectorDetailPage } from './components/connector-detail.js';
import { MarketplacePage }    from './components/marketplace.js';
import { CatalogPage }        from './components/catalog.js';
import { ProfilePage }        from './components/profile-page.js';
import { ApprovalGroupsPage } from './components/approval-groups.js';
import { ApprovalRulesPage }  from './components/approval-rules.js';
import { ConfigPage }         from './components/config-page.js';
import { AgentInboxPage }     from './components/agent-inbox.js';
import { HomePage }           from './components/home-page.js';
import { LlmRequestsPage }   from './components/llm-requests.js';
import { LlmRequestDetail }  from './components/llm-request-detail.js';
import { SessionDetailPage } from './components/session-detail.js';
import { TicSessionsPage }  from './components/tic-sessions.js';
import { ProjectsPage }     from './components/projects/index.js';
import { FileViewerPage }   from './components/file-viewer-page.js';
import { SetupPage }        from './components/setup-page.js';
import { LoginPage }        from './components/login-page.js';

// Register the global `openFile(path)` helper (window.openFile → location.hash).
import './lib/open-file.js';

customElements.define('app-topbar',           AppTopbar);
customElements.define('app-sidebar',          AppSidebar);
customElements.define('app-copilot',          AppCopilot);
customElements.define('llm-providers-page',   LlmProvidersPage);
customElements.define('models-hub-page',           ModelsHubPage);
customElements.define('models-llm-section',        ModelsLlmSection);
customElements.define('models-transcribe-section', ModelsTranscribeSection);
customElements.define('models-image-section',      ModelsImageSection);
customElements.define('models-tts-section',        ModelsTtsSection);
customElements.define('tasks-page',            TasksPage);
customElements.define('agents-page',          AgentsPage);
customElements.define('users-page',           UsersPage);
customElements.define('roles-page',           RolesPage);
customElements.define('connectors-page',      ConnectorsPage);
customElements.define('connector-detail-page', ConnectorDetailPage);
customElements.define('marketplace-page',     MarketplacePage);
customElements.define('catalog-page',         CatalogPage);
customElements.define('profile-page',         ProfilePage);
customElements.define('approval-groups-page', ApprovalGroupsPage);
customElements.define('approval-rules-page',  ApprovalRulesPage);
customElements.define('config-page',          ConfigPage);
customElements.define('agent-inbox-page',     AgentInboxPage);
customElements.define('home-page',            HomePage);
customElements.define('llm-requests-page',   LlmRequestsPage);
customElements.define('llm-request-detail',  LlmRequestDetail);
customElements.define('session-detail-page', SessionDetailPage);
customElements.define('tic-sessions-page',   TicSessionsPage);
customElements.define('projects-page',       ProjectsPage);
customElements.define('file-viewer-page', FileViewerPage);
customElements.define('setup-page',       SetupPage);
customElements.define('login-page',       LoginPage);

// Toggle the workspace placeholder when an LLM page opens/closes.
const workspace = document.getElementById('app-workspace');
window.addEventListener('llm-page-change', (e) => {
  workspace.style.display = e.detail.page ? 'none' : 'flex';
});

// ── First-run check ─────────────────────────────────────────────────────────
// If no user exists yet, show the setup screen. Otherwise, check if we have
// a valid session: if not, show the login screen. If logged in, show the app.
(async () => {
  const app    = document.getElementById('app');
  const setup  = document.querySelector('setup-page');
  const login  = document.querySelector('login-page');

  try {
    const setupRes = await fetch('/api/setup/status');
    if (setupRes.ok) {
      const { needs_setup } = await setupRes.json();
      if (needs_setup) {
        if (app)   app.style.display = 'none';
        if (setup) setup.style.display = '';
        return;
      }
    }
  } catch { /* fall through to auth check */ }

  try {
    const meRes = await fetch('/api/auth/me');
    if (meRes.status === 401) {
      if (app)   app.style.display = 'none';
      if (login) login.style.display = '';
      return;
    }
  } catch { /* show app by default */ }
})();
