export function browserCapabilities(window, document) {
  const firefox = /Firefox\/|FxiOS\//i.test(window.navigator?.userAgent || '');
  const files = window.isSecureContext === true
    && typeof window.navigator?.serviceWorker?.register === 'function'
    && (firefox || typeof window.streamSaver?.createWriteStream === 'function');
  return { files, folders: files && !firefox && 'webkitdirectory' in document.createElement('input') };
}
