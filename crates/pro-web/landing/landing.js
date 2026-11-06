/* Vuuut landing page — progressive enhancement only.
   The page is complete without this file: every `.reveal` element is visible
   if the observer never runs, because the stylesheet only hides them for
   browsers that get here and the fallback below unhides them immediately if
   IntersectionObserver is missing. */
(function () {
  'use strict';

  var reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  /* Reveal on scroll. One observer for the whole page; each element keeps the
     stagger it was given in markup through its own --d. */
  var targets = document.querySelectorAll('.reveal');
  if (reduced || !('IntersectionObserver' in window)) {
    targets.forEach(function (el) { el.classList.add('in'); });
  } else {
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (!entry.isIntersecting) return;
        entry.target.classList.add('in');
        io.unobserve(entry.target);
      });
    }, { rootMargin: '0px 0px -12% 0px', threshold: 0.08 });
    targets.forEach(function (el) { io.observe(el); });
  }

  /* The masthead only grows its rule once the page has moved, so the top of
     the page reads as one uninterrupted sheet. */
  var mast = document.querySelector('.masthead');
  if (mast) {
    var onScroll = function () { mast.classList.toggle('stuck', window.scrollY > 8); };
    onScroll();
    window.addEventListener('scroll', onScroll, { passive: true });
  }

  /* Keep the footer year honest without server rendering. */
  var yr = document.getElementById('yr');
  if (yr) yr.textContent = String(new Date().getFullYear());
})();
