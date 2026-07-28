/**
 * Background service worker.
 *
 * It does one thing: open the viewer when the toolbar icon is clicked.
 *
 * It stays that way deliberately. An MV3 service worker is terminated after ~30
 * seconds of idle and restarted on demand, which makes it the worst possible
 * place to hold a parsed file, an index, or anything else with a lifetime. All
 * engine state lives in the viewer tab's dedicated Worker, which lives exactly
 * as long as the tab that owns it.
 *
 * Note the manifest requests **no** permissions: `chrome.tabs.create` does not
 * need the `tabs` permission, and the privacy claim ("nothing leaves your
 * machine") is worth more when it is verifiable from the manifest alone than
 * when it is asserted in a store listing.
 */

const VIEWER_PAGE = 'viewer.html';

chrome.action.onClicked.addListener(() => {
  void chrome.tabs.create({ url: chrome.runtime.getURL(VIEWER_PAGE) });
});
