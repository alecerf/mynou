import { $, h, icon } from './dom.js';
import { quality, openDetail, act, toast, skeleton } from './components.js';
import { Motion } from './motion.js';
import { link } from './link.js';
import { itemsOf } from './store.js';

// Manual search: the releases the indexers offer for one film or episode, and
// the one the operator picks. Mynou would choose the first acceptable release
// by itself; a rejected one can still be taken after a second click.

/** Searches with a film or episode request, as a job holds it. */
export const requestSearch = (request) => ({
  search: (after) => link.call('POST', `/api/search${after ? `?limit=100&after=${encodeURIComponent(after)}` : ''}`, request),
  grab: (id) => link.call('POST', '/api/grab', { request, candidate_id: id }),
});

/** Searches with an episode of a followed series, which the service builds. */
export const episodeSearch = (seriesId, season, episode) => {
  const path = `/api/series/${encodeURIComponent(seriesId)}`;
  return {
    search: (after) => link.call('POST', `${path}/search`, after ? { season, episode, after, limit: 100 } : { season, episode }),
    grab: (id) => link.call('POST', `${path}/grab`, { season, episode, candidate_id: id }),
  };
};

/** Opens the release list of one title. `back` returns to the sheet it came from. */
export async function openReleases(title, source, back) {
  const list = h('ol', { class: 'release-list' }, skeleton('rows', 5));
  const rejectedList = h('ol', { class: 'release-list' });
  const rejectedSummary = h('summary');
  const rejected = h('details', { class: 'release-rejected', hidden: true }, rejectedSummary, rejectedList);
  const summary = h('p', { class: 'detail-sub searching' }, 'Searching the indexers…');
  const more = h('button', { type: 'button', hidden: true }, 'Load more releases');
  const counts = { accepted: 0, rejected: 0 };
  let next = null;
  const grab = async (button, candidate) => {
    const accepted = candidate.assessment?.accepted;
    if (!accepted && button.dataset.confirm !== 'yes') {
      button.dataset.confirm = 'yes';
      button.replaceChildren(icon('alert'), 'Download anyway');
      Motion.nudge(button);
      return;
    }
    await act(button, async () => {
      await source.grab(candidate.id);
      $('#detail').close();
      toast('Download started');
    });
  };
  let pick = null;
  const add = (report) => {
    const accepted = itemsOf(report.accepted);
    const refused = itemsOf(report.rejected);
    pick ??= report.selected_candidate_id ?? null;
    list.querySelector(':scope > .loading')?.remove();
    summary.classList.remove('searching');
    const before = list.childElementCount + rejectedList.childElementCount;
    counts.accepted += accepted.length;
    counts.rejected += refused.length;
    list.append(...accepted.map((c) => releaseRow(c, grab, c.id === pick)));
    rejectedList.append(...refused.map((c) => releaseRow(c, grab, false)));
    next = report.next ?? null;
    const total = Number(report.candidate_count) || 0;
    const indexers = report.indexers ?? {};
    summary.textContent = [
      `${counts.accepted} ${counts.accepted === 1 ? 'release fits' : 'releases fit'} your profile`, counts.rejected ? `${counts.rejected} rejected` : null,
      total > counts.accepted + counts.rejected ? `${total} found` : null,
      Number(indexers.failed) ? `${indexers.failed} of ${indexers.configured} indexers did not answer` : null,
    ].filter(Boolean).join(' · ');
    if (!counts.accepted) list.replaceChildren(h('li', { class: 'empty' }, 'No release meets your profile. A rejected one can still be downloaded.'));
    rejected.hidden = !counts.rejected;
    rejectedSummary.textContent = `${counts.rejected} rejected by your profile`;
    more.hidden = !next;
    Motion.arrive([...list.children, ...rejectedList.children].slice(before));
  };
  more.addEventListener('click', () => act(more, async () => { if (next) add(await source.search(next)); }));
  openDetail(`Releases`,
    back ? h('button', { type: 'button', class: 'text-link back', onclick: back.run }, `← ${back.label}`) : null,
    back ? null : h('p', { class: 'releases-of' }, title), summary, list, rejected, h('div', { class: 'actions' }, more));
  try { add(await source.search(null)); } catch (error) {
    list.querySelector(':scope > .loading')?.remove();
    summary.classList.remove('searching');
    summary.textContent = error.message; summary.dataset.tone = 'bad';
  }
}

/** One release: title, indexer, seeders, what was read from its title and
    why it was refused. The release Mynou would choose is marked. */
function releaseRow(c, grab, chosen) {
  const assessment = c.assessment ?? {};
  const rank = assessment.rank ?? {};
  // The rank holds positions in the profile's lists; the attributes hold what was read from the title.
  const attributes = assessment.attributes ?? {};
  const traits = [Number(attributes.resolution) ? `${attributes.resolution}p` : null, attributes.source, attributes.codec,
    ...(Array.isArray(attributes.languages) ? attributes.languages : [])].filter((v) => typeof v === 'string' && v);
  const reasons = Array.isArray(assessment.reasons) ? assessment.reasons.filter((r) => typeof r === 'string') : [];
  const download = h('button', { type: 'button', class: chosen ? 'primary' : 'secondary' }, icon('down'), 'Download');
  download.addEventListener('click', () => grab(download, c));
  const seeders = Number(c.seeders);
  return h('li', { class: `release${chosen ? ' chosen' : ''}${assessment.accepted ? '' : ' refused'}` },
    h('div', { class: 'release-main' },
      chosen ? h('span', { class: 'badge', dataset: { tone: 'accent' } }, 'Mynou\'s pick') : null,
      h('div', { class: 'release-title' }, c.title ?? '(untitled)'),
      h('div', { class: 'release-facts' },
        traits.length ? quality(traits) : null,
        h('span', {}, c.source),
        typeof rank.custom_score === 'number' && rank.custom_score ? h('span', {}, `score ${rank.custom_score}`) : null),
      reasons.length ? h('div', { class: 'release-why' }, reasons.join(' · ')) : null),
    h('div', { class: 'release-side' },
      h('span', { class: 'seeders', dataset: { tone: seeders ? '' : 'bad' }, title: 'Seeders' }, h('strong', {}, Number.isFinite(seeders) ? String(seeders) : String(c.seeders ?? '?')), ' seeders'),
      download));
}
