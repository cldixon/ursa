// The theme before the first paint: the one picked here before, else the
// system's. A file of its own, not inline: the Content Security Policy allows
// only the site's own scripts (src/headers.ts).
try {
  const saved = localStorage.getItem("ursa-theme");
  if (saved === "light" || saved === "dark") document.documentElement.dataset.theme = saved;
} catch {}
