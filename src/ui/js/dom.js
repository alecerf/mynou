// DOM building blocks. Strings only ever become text nodes.

export const $ = (selector, root = document) => root.querySelector(selector);

/** Tiny DOM builder. Strings become text nodes, never markup. */
export function h(tag, props = {}, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props ?? {})) {
    if (value === undefined || value === null || value === false) continue;
    if (key === 'class') el.className = value;
    else if (key === 'dataset') Object.assign(el.dataset, value);
    else if (key.startsWith('on')) el.addEventListener(key.slice(2), value);
    else if (key in el && key !== 'list') el[key] = value;
    else el.setAttribute(key, value === true ? '' : value);
  }
  el.append(...children.flat().filter((c) => c !== null && c !== undefined && c !== false));
  return el;
}
/** An icon from the sprite in index.html, cloned so no SVG namespace lives in this file. */
export function icon(name, className = 'icon') {
  const svg = $('#icon-template').content.firstElementChild.cloneNode(true);
  svg.setAttribute('class', className);
  svg.firstElementChild.setAttribute('href', `#i-${name}`);
  return svg;
}
/** Updates `container` so it holds one element per item, in order, reusing existing nodes by key. */
export function reconcile(container, items, key, create, update) {
  const existing = new Map([...container.children].map((el) => [el.dataset.key, el]));
  let cursor = container.firstElementChild;
  for (const [index, item] of items.entries()) {
    const k = String(key(item, index));
    let el = existing.get(k);
    if (el) existing.delete(k);
    else { el = create(item); el.dataset.key = k; }
    update(el, item);
    if (el !== cursor) container.insertBefore(el, cursor);
    else cursor = cursor.nextElementSibling;
  }
  for (const el of existing.values()) el.remove();
}
