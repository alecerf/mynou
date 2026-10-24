// The texture and light of the covers: a film grain drawn once in this page,
// and a cover that leans toward the pointer. Nothing is fetched.

/** A tile of monochrome noise, kept as a data URL in `--grain` for the CSS. */
function grain() {
  const size = 128;
  const canvas = document.createElement('canvas');
  canvas.width = canvas.height = size;
  const context = canvas.getContext('2d');
  if (!context) return;
  const image = context.createImageData(size, size);
  for (let i = 0; i < image.data.length; i += 4) {
    const value = Math.random() * 255;
    image.data.set([value, value, value, 255], i);
  }
  context.putImageData(image, 0, 0);
  document.documentElement.style.setProperty('--grain', `url("${canvas.toDataURL('image/png')}")`);
}

/** The cover under a fine pointer tilts a few degrees and catches the light where the pointer is. */
function tilt() {
  const allowed = matchMedia('(hover: hover) and (pointer: fine) and (prefers-reduced-motion: no-preference)');
  let current = null;
  const reset = () => {
    if (!current) return;
    for (const name of ['--rx', '--ry', '--mx', '--my']) current.style.removeProperty(name);
    current = null;
  };
  addEventListener('pointermove', (event) => {
    const cover = allowed.matches && event.target.closest?.('.media-card .cover, .motion-card:not(.queue-card)');
    if (cover !== current) reset();
    if (!cover) return;
    current = cover;
    const box = cover.getBoundingClientRect();
    const x = (event.clientX - box.left) / box.width, y = (event.clientY - box.top) / box.height;
    // A large card leans less than a poster.
    const range = box.width > 400 ? 1.5 : 6;
    cover.style.setProperty('--rx', `${((0.5 - y) * range).toFixed(2)}deg`);
    cover.style.setProperty('--ry', `${((x - 0.5) * range).toFixed(2)}deg`);
    cover.style.setProperty('--mx', `${(x * 100).toFixed(1)}%`);
    cover.style.setProperty('--my', `${(y * 100).toFixed(1)}%`);
  }, { passive: true });
  document.addEventListener('pointerleave', reset);
}

export function setupArt() {
  grain();
  tilt();
}
