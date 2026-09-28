/** Scroll-spy for the desktop table of contents. */

import { mount } from "@cloudflare/nimbus-docs/client";

const READING_BAND = 0.25;
const BOTTOM_EPSILON = 2;

function initToc(root: HTMLElement): () => void {
  const nav = root.querySelector<HTMLElement>("nav");
  const links = root.querySelectorAll<HTMLElement>("[data-nb-toc-link]");
  if (!nav || links.length === 0) return () => {};

  const slugs = Array.from(links).map((l) => l.dataset.nbSlug!);
  // Observe only resolvable headings, each carrying its original index, so
  // scroll-spy stays aligned with the full link list even when a
  // heading slugs to "" (e.g. emoji-only `## 🎉`) and has no DOM target.
  const observed = slugs
    .map((slug, index) => ({ el: document.getElementById(slug), index }))
    .filter((o): o is { el: HTMLElement; index: number } => o.el !== null);
  if (observed.length === 0) return () => {};
  const indexOfEl = new Map<HTMLElement, number>(
    observed.map((o) => [o.el, o.index]),
  );

  let currentIndex = -1;
  let currentLink: HTMLElement | null = null;

  function setActive(index: number) {
    if (index === currentIndex) return;
    currentIndex = index;

    currentLink?.removeAttribute("aria-current");
    const activeLink = links[index] ?? null;
    activeLink?.setAttribute("aria-current", "true");
    currentLink = activeLink;
  }

  const inBand = new Set<number>();
  let observedIndex = 0;
  let atBottom = false;
  let pinnedIndex: number | null = null;
  let pinnedEnteredViewport = false;

  function resolve() {
    if (pinnedIndex !== null) {
      setActive(pinnedIndex);
      return;
    }
    setActive(atBottom ? links.length - 1 : observedIndex);
  }

  // rootMargin collapses the root to the top band; deepest in-band heading wins.
  const spy = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        const i = indexOfEl.get(entry.target as HTMLElement);
        if (i === undefined) continue;
        if (entry.isIntersecting) inBand.add(i);
        else inBand.delete(i);
      }
      if (inBand.size > 0) observedIndex = Math.max(...inBand);
      resolve();
    },
    { rootMargin: `0px 0px -${(1 - READING_BAND) * 100}% 0px`, threshold: 0 },
  );
  observed.forEach((o) => spy.observe(o.el));

  function updateBottom() {
    const scrollEl = document.scrollingElement ?? document.documentElement;
    const maxScroll = scrollEl.scrollHeight - window.innerHeight;
    const next =
      maxScroll > BOTTOM_EPSILON &&
      scrollEl.scrollTop >= maxScroll - BOTTOM_EPSILON;
    if (next !== atBottom) {
      atBottom = next;
      resolve();
    }
  }

  function updateObservedIndex() {
    const bandBottom = window.innerHeight * READING_BAND;
    let nextIndex = 0;
    for (const o of observed) {
      if (o.el.getBoundingClientRect().top <= bandBottom) nextIndex = o.index;
      else break;
    }
    observedIndex = nextIndex;
  }

  function releaseStalePin() {
    if (pinnedIndex === null) return;
    const heading = document.getElementById(slugs[pinnedIndex]);
    if (!heading) {
      pinnedIndex = null;
      pinnedEnteredViewport = false;
      return;
    }

    const rect = heading.getBoundingClientRect();
    const inViewport = rect.bottom >= 0 && rect.top <= window.innerHeight;
    if (inViewport) {
      pinnedEnteredViewport = true;
      return;
    }

    if (pinnedEnteredViewport) {
      pinnedIndex = null;
      pinnedEnteredViewport = false;
    }
  }

  let ticking = false;
  function onScroll() {
    if (ticking) return;
    ticking = true;
    requestAnimationFrame(() => {
      updateObservedIndex();
      updateBottom();
      releaseStalePin();
      resolve();
      ticking = false;
    });
  }

  const controller = new AbortController();

  nav.addEventListener(
    "click",
    (e) => {
      if (
        e.defaultPrevented ||
        e.button !== 0 ||
        e.metaKey ||
        e.ctrlKey ||
        e.shiftKey ||
        e.altKey
      )
        return;
      const link = (e.target as Element).closest<HTMLElement>(
        "[data-nb-toc-link]",
      );
      if (!link) return;
      const i = slugs.indexOf(link.dataset.nbSlug!);
      if (i === -1) return;
      pinnedIndex = i;
      const heading = document.getElementById(slugs[i]);
      const rect = heading?.getBoundingClientRect();
      pinnedEnteredViewport =
        !!rect && rect.bottom >= 0 && rect.top <= window.innerHeight;
      resolve();
    },
    { signal: controller.signal },
  );

  // Hand-driven scrolling releases the pin and resumes auto-tracking.
  function releasePin() {
    if (pinnedIndex === null) return;
    pinnedIndex = null;
    pinnedEnteredViewport = false;
    resolve();
  }
  const NAV_KEYS = new Set([
    "ArrowUp",
    "ArrowDown",
    "PageUp",
    "PageDown",
    "Home",
    "End",
    " ",
    "Spacebar",
  ]);
  window.addEventListener("wheel", releasePin, {
    passive: true,
    signal: controller.signal,
  });
  window.addEventListener("touchmove", releasePin, {
    passive: true,
    signal: controller.signal,
  });
  window.addEventListener(
    "keydown",
    (e) => {
      if (NAV_KEYS.has(e.key)) releasePin();
    },
    { signal: controller.signal },
  );

  window.addEventListener("scroll", onScroll, {
    passive: true,
    signal: controller.signal,
  });
  updateObservedIndex();
  updateBottom();
  resolve();

  return () => {
    controller.abort();
    spy.disconnect();
  };
}

mount("[data-nb-toc]", initToc);
