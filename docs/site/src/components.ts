/**
 * MDX globals registry — components available inside MDX without `import`. Wired via `<Content
 * components={components} />` in `[...slug].astro`. Add new components here as you build (or
 * install) them.
 */

import {
  Aside,
  Card,
  CardGrid,
  FlowDiagram,
  PackageManagers,
  Step,
  Steps,
  TabItem,
  Tabs,
} from "./components/mdx";

export const components = {
  Aside,
  Card,
  CardGrid,
  FlowDiagram,
  PackageManagers,
  Step,
  Steps,
  TabItem,
  Tabs,
};
