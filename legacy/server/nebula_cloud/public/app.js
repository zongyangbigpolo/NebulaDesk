const ACCESS_TOKEN_KEY = 'nebulaCloud.accessToken';
const REFRESH_TOKEN_KEY = 'nebulaCloud.refreshToken';

function getAccessToken() {
  return localStorage.getItem(ACCESS_TOKEN_KEY);
}

function getRefreshToken() {
  return localStorage.getItem(REFRESH_TOKEN_KEY);
}

function storeTokens(payload) {
  localStorage.setItem(ACCESS_TOKEN_KEY, payload.accessToken);
  localStorage.setItem(REFRESH_TOKEN_KEY, payload.refreshToken);
}

function clearTokens() {
  localStorage.removeItem(ACCESS_TOKEN_KEY);
  localStorage.removeItem(REFRESH_TOKEN_KEY);
}

async function refreshTokens() {
  const refreshToken = getRefreshToken();
  if (!refreshToken) {
    clearTokens();
    window.location.href = '/';
    return false;
  }

  const response = await fetch('/auth/refresh', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ refreshToken }),
  });

  if (!response.ok) {
    clearTokens();
    window.location.href = '/';
    return false;
  }

  const payload = await response.json();
  storeTokens(payload);
  return true;
}

async function fetchApi(url, options = {}, allowRetry = true) {
  const headers = new Headers(options.headers || {});
  if (!headers.has('Content-Type') && options.body) {
    headers.set('Content-Type', 'application/json');
  }
  const accessToken = getAccessToken();
  if (accessToken) {
    headers.set('Authorization', `Bearer ${accessToken}`);
  }

  const response = await fetch(url, { ...options, headers });
  if (response.status === 401 && allowRetry && getRefreshToken()) {
    const refreshed = await refreshTokens();
    if (refreshed) {
      return fetchApi(url, options, false);
    }
  }
  return response;
}

function pretty(data) {
  return JSON.stringify(data, null, 2);
}

function setOutput(id, value) {
  const element = document.getElementById(id);
  if (element) {
    element.textContent = typeof value === 'string' ? value : pretty(value);
  }
}

async function readJson(response) {
  const text = await response.text();
  if (!text) {
    return null;
  }
  return JSON.parse(text);
}

function renderError(payload) {
  return payload?.message || pretty(payload || { message: 'Unknown error' });
}

async function handleRegister(event) {
  event.preventDefault();
  const form = event.currentTarget;
  const payload = Object.fromEntries(new FormData(form).entries());
  const response = await fetch('/auth/register', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });
  const json = await readJson(response);
  if (!response.ok) {
    setOutput('register-output', renderError(json));
    return;
  }
  setOutput('register-output', json);
  form.reset();
}

async function handleLogin(event) {
  event.preventDefault();
  const form = event.currentTarget;
  const payload = Object.fromEntries(new FormData(form).entries());
  const response = await fetch('/auth/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });
  const json = await readJson(response);
  if (!response.ok) {
    setOutput('login-output', renderError(json));
    return;
  }
  storeTokens(json);
  setOutput('login-output', { message: 'Login successful. Redirecting…' });
  window.location.href = '/dashboard';
}

function deviceCard(device) {
  const ownership = device.role === 'OWNER';
  return `
    <article class="device-card" data-device-id="${device.id}">
      <header class="device-card-header">
        <div>
          <h3>${device.name}</h3>
          <p><strong>Role:</strong> ${device.role} · <strong>Relay device:</strong> <code>${device.relayDeviceId}</code></p>
          <p><strong>Online:</strong> ${device.online ? 'yes' : 'no'} · <strong>Last seen:</strong> ${device.lastSeenAt || 'never'}</p>
        </div>
        <div class="inline-actions">
          <button data-action="connect">Connect</button>
          ${ownership ? '<button data-action="load-grants" class="secondary">Load grants</button>' : ''}
          ${ownership ? '<button data-action="delete-device" class="danger">Delete</button>' : ''}
        </div>
      </header>
      <pre class="output" data-output="connect"></pre>
      ${ownership ? `
        <section class="grant-section">
          <form data-action="grant-form" class="stacked-form compact">
            <label>Invite email<input name="granteeEmail" type="email" required /></label>
            <label>Role
              <select name="role">
                <option value="VIEWER">VIEWER</option>
                <option value="CONTROLLER">CONTROLLER</option>
              </select>
            </label>
            <button type="submit">Grant access</button>
          </form>
          <div data-output="grants"></div>
        </section>
      ` : ''}
    </article>
  `;
}

async function loadProfile() {
  const response = await fetchApi('/auth/me');
  if (!response.ok) {
    clearTokens();
    window.location.href = '/';
    return;
  }
  const json = await readJson(response);
  document.getElementById('whoami').textContent = `Signed in as ${json.user.displayName} (${json.user.email})`;
}

async function loadDevices() {
  const response = await fetchApi('/devices');
  const json = await readJson(response);
  if (!response.ok) {
    document.getElementById('device-list').innerHTML = `<pre class="output">${renderError(json)}</pre>`;
    return;
  }

  const devices = json.devices || [];
  const list = document.getElementById('device-list');
  if (devices.length === 0) {
    list.innerHTML = '<p>No devices yet. Create one above.</p>';
    return;
  }

  list.innerHTML = devices.map(deviceCard).join('');
  attachDeviceCardHandlers();
}

async function handleCreateDevice(event) {
  event.preventDefault();
  const payload = Object.fromEntries(new FormData(event.currentTarget).entries());
  const response = await fetchApi('/devices', {
    method: 'POST',
    body: JSON.stringify(payload),
  });
  const json = await readJson(response);
  if (!response.ok) {
    setOutput('create-device-output', renderError(json));
    return;
  }
  setOutput('create-device-output', json);
  event.currentTarget.reset();
  await loadDevices();
}

async function handleRedeemClaim(event) {
  event.preventDefault();
  const payload = Object.fromEntries(new FormData(event.currentTarget).entries());
  const response = await fetchApi('/device-claims/redeem', {
    method: 'POST',
    body: JSON.stringify(payload),
  });
  const json = await readJson(response);
  if (!response.ok) {
    setOutput('redeem-claim-output', renderError(json));
    return;
  }
  setOutput('redeem-claim-output', json);
  event.currentTarget.reset();
}

function attachDeviceCardHandlers() {
  document.querySelectorAll('.device-card').forEach((card) => {
    const deviceId = card.getAttribute('data-device-id');
    card.querySelector('[data-action="connect"]')?.addEventListener('click', () => {
      window.location.href = `/watch?device=${encodeURIComponent(deviceId)}&name=${encodeURIComponent(card.querySelector('h3').textContent)}`;
    });

    card.querySelector('[data-action="delete-device"]')?.addEventListener('click', async () => {
      const response = await fetchApi(`/devices/${deviceId}`, { method: 'DELETE' });
      if (response.ok) {
        await loadDevices();
      } else {
        const json = await readJson(response);
        card.querySelector('[data-output="connect"]').textContent = renderError(json);
      }
    });

    card.querySelector('[data-action="load-grants"]')?.addEventListener('click', async () => {
      await loadGrants(card, deviceId);
    });

    card.querySelector('[data-action="grant-form"]')?.addEventListener('submit', async (event) => {
      event.preventDefault();
      const payload = Object.fromEntries(new FormData(event.currentTarget).entries());
      const response = await fetchApi(`/devices/${deviceId}/grants`, {
        method: 'POST',
        body: JSON.stringify(payload),
      });
      const json = await readJson(response);
      if (!response.ok) {
        card.querySelector('[data-output="grants"]').innerHTML = `<pre class="output">${renderError(json)}</pre>`;
        return;
      }
      event.currentTarget.reset();
      await loadGrants(card, deviceId);
    });
  });
}

async function loadGrants(card, deviceId) {
  const response = await fetchApi(`/devices/${deviceId}/grants`);
  const json = await readJson(response);
  const container = card.querySelector('[data-output="grants"]');
  if (!response.ok) {
    container.innerHTML = `<pre class="output">${renderError(json)}</pre>`;
    return;
  }
  const grants = json.grants || [];
  if (grants.length === 0) {
    container.innerHTML = '<p>No active grants.</p>';
    return;
  }
  container.innerHTML = grants.map((grant) => `
    <div class="grant-item">
      <div>
        <strong>${grant.granteeUser.displayName}</strong> (${grant.granteeUser.email}) · ${grant.role}
      </div>
      <button data-revoke-grant="${grant.id}" class="danger">Revoke</button>
    </div>
  `).join('');
  container.querySelectorAll('[data-revoke-grant]').forEach((button) => {
    button.addEventListener('click', async () => {
      await fetchApi(`/grants/${button.getAttribute('data-revoke-grant')}`, { method: 'DELETE' });
      await loadGrants(card, deviceId);
      await loadDevices();
    });
  });
}

async function logout() {
  const refreshToken = getRefreshToken();
  if (refreshToken) {
    await fetch('/auth/logout', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ refreshToken }),
    });
  }
  clearTokens();
  window.location.href = '/';
}

function initAuthPage() {
  if (getAccessToken()) {
    window.location.href = '/dashboard';
    return;
  }
  document.getElementById('register-form').addEventListener('submit', handleRegister);
  document.getElementById('login-form').addEventListener('submit', handleLogin);
}

async function initDashboardPage() {
  if (!getAccessToken() && !(await refreshTokens())) {
    return;
  }
  document.getElementById('logout').addEventListener('click', logout);
  document.getElementById('refresh-devices').addEventListener('click', loadDevices);
  document.getElementById('create-device-form').addEventListener('submit', handleCreateDevice);
  document.getElementById('redeem-claim-form').addEventListener('submit', handleRedeemClaim);
  await loadProfile();
  await loadDevices();
}

window.addEventListener('DOMContentLoaded', () => {
  if (window.location.pathname === '/dashboard') {
    void initDashboardPage();
  } else if (window.location.pathname === '/watch') {
    // watch.js drives its own initialization (WebRTC session), but still
    // relies on this file's fetchApi/token helpers.
  } else {
    initAuthPage();
  }
});
