const baseUrl = process.env.NEBULA_CLOUD_URL || 'http://127.0.0.1:4000';

async function request(path, options = {}) {
  const headers = {
    ...(options.headers || {}),
  };
  if (options.body && !headers['Content-Type']) {
    headers['Content-Type'] = 'application/json';
  }
  const response = await fetch(`${baseUrl}${path}`, {
    ...options,
    headers,
  });
  const text = await response.text();
  const body = text ? JSON.parse(text) : null;
  if (!response.ok) {
    throw new Error(`${response.status} ${path}: ${JSON.stringify(body)}`);
  }
  return body;
}

async function main() {
  const suffix = Date.now();
  const ownerEmail = `owner-${suffix}@example.com`;
  const viewerEmail = `viewer-${suffix}@example.com`;
  const password = 'super-secret-pass';

  await request('/auth/register', { method: 'POST', body: JSON.stringify({ email: ownerEmail, password, displayName: 'Owner' }) });
  await request('/auth/register', { method: 'POST', body: JSON.stringify({ email: viewerEmail, password, displayName: 'Viewer' }) });

  const ownerLogin = await request('/auth/login', { method: 'POST', body: JSON.stringify({ email: ownerEmail, password }) });
  const viewerLogin = await request('/auth/login', { method: 'POST', body: JSON.stringify({ email: viewerEmail, password }) });

  const createDevice = await request('/devices', {
    method: 'POST',
    headers: { Authorization: `Bearer ${ownerLogin.accessToken}` },
    body: JSON.stringify({ name: 'Smoke Flow Mac' }),
  });

  await request(`/devices/${createDevice.device.id}/grants`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${ownerLogin.accessToken}` },
    body: JSON.stringify({ granteeEmail: viewerEmail, role: 'VIEWER' }),
  });

  const viewerDevices = await request('/devices', {
    headers: { Authorization: `Bearer ${viewerLogin.accessToken}` },
  });

  const connect = await request(`/devices/${createDevice.device.id}/connect`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${viewerLogin.accessToken}` },
  });

  const authorize = await request('/internal/authorize', {
    method: 'POST',
    headers: { 'X-Relay-Secret': process.env.RELAY_SHARED_SECRET || 'compose-relay-shared-secret-1234' },
    body: JSON.stringify({ deviceId: connect.relayDeviceId, token: connect.sessionToken }),
  });

  console.log(JSON.stringify({
    ownerEmail,
    viewerEmail,
    deviceId: createDevice.device.id,
    viewerDeviceCount: viewerDevices.devices.length,
    authorized: authorize.authorized,
    claimCode: createDevice.registration.claimCode,
    relayDeviceId: connect.relayDeviceId,
  }, null, 2));
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
