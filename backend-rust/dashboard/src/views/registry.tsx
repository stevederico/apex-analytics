// Tab registries by header switcher scope:
//   native → Dottie desktop telemetry tabs
//   site   → multi-tenant web analytics
//   repo   → GitHub growth overview for one owner/name
import type { ComponentType } from 'react';

import Overview from './Overview';
import Lifecycle from './Lifecycle';
import Funnels from './Funnels';
import Issues from './Issues';
import Audience from './Audience';
import Landing from './Landing';
import Users from './Users';
import Growth from './Growth';
import Feedback from './Feedback';

import SiteOverview from './site/Overview';
import SiteEvents from './site/Events';

export interface Tab {
  id: string;
  label: string;
  View: ComponentType<{ site: string }>;
}

export const NATIVE_TABS: Tab[] = [
  { id: 'overview', label: 'Overview', View: Overview },
  { id: 'lifecycle', label: 'Lifecycle', View: Lifecycle },
  { id: 'funnels', label: 'Funnels', View: Funnels },
  { id: 'issues', label: 'Issues', View: Issues },
  { id: 'audience', label: 'Audience', View: Audience },
  { id: 'landing', label: 'Landing', View: Landing },
  { id: 'users', label: 'Users', View: Users },
  { id: 'feedback', label: 'Feedback', View: Feedback },
];

export const SITE_TABS: Tab[] = [
  { id: 'site_overview', label: 'Overview', View: SiteOverview },
  { id: 'site_events', label: 'Events', View: SiteEvents },
  { id: 'site_feedback', label: 'Feedback', View: Feedback },
];

/** One overview = growth metrics for the selected GitHub repo. */
export const REPO_TABS: Tab[] = [
  { id: 'repo_overview', label: 'Overview', View: Growth },
];
