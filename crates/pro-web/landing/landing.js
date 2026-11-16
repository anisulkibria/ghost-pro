/* Vuuut landing page — progressive enhancement only.
   The page is complete without this file. Sections rise into view through a
   CSS view timeline where the browser has one; this script only supplies the
   same effect to browsers that do not, and it hides nothing unless it is
   also there to reveal it (the stylesheet keys that off `html.lp-io`, which
   is set here). */
(function () {
  'use strict';

  var reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  var native = window.CSS && CSS.supports && CSS.supports('animation-timeline: view()');

  if (!reduced && !native && 'IntersectionObserver' in window) {
    document.documentElement.classList.add('lp-io');
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (!entry.isIntersecting) return;
        entry.target.classList.add('in');
        io.unobserve(entry.target);
      });
    }, { rootMargin: '0px 0px -10% 0px', threshold: 0.06 });
    document.querySelectorAll('.lp-rise').forEach(function (el) { io.observe(el); });
  }

  /* Keep the footer year honest without server rendering. */
  var yr = document.getElementById('yr');
  if (yr) yr.textContent = String(new Date().getFullYear());
})();
