import 'package:flutter/material.dart';

import 'cloud_models.dart';
import 'cloud_session_controller.dart';
import 'models.dart';
import 'session_launcher.dart';

// "Cloud" tab: log into a nebula_cloud instance and see/connect to every VDA
// the account can reach (owned, granted, via a Group, or all of them for an
// admin) — see server/nebula_cloud/README.md. Connecting needs nothing typed
// in beyond the login itself: relay routing, the short-lived session ticket,
// and the PSK all come from POST /devices/:id/connect. The login itself is
// owned by CloudSessionController and shared with the "Host this Mac" tab
// (host_tab.dart) — one sign-in, both capabilities.
class CloudTab extends StatefulWidget {
  final CloudSessionController controller;
  final SessionLauncher launcher;
  const CloudTab({super.key, required this.controller, required this.launcher});

  @override
  State<CloudTab> createState() => _CloudTabState();
}

class _CloudTabState extends State<CloudTab> {
  List<CloudDevice> _devices = [];
  bool _loadingDevices = false;
  String? _error;

  // device id -> live session state / pid, mirrors HomePage's per-VDA maps.
  final Map<String, SessionState> _states = {};
  final Map<String, int> _pids = {};

  @override
  void initState() {
    super.initState();
    widget.controller.addListener(_onControllerChanged);
    if (widget.controller.session != null) _refreshDevices();
  }

  @override
  void dispose() {
    widget.controller.removeListener(_onControllerChanged);
    super.dispose();
  }

  void _onControllerChanged() {
    if (widget.controller.session != null && _devices.isEmpty && !_loadingDevices) {
      _refreshDevices();
    }
    setState(() {});
  }

  Future<void> _refreshDevices() async {
    final session = widget.controller.session;
    if (session == null) return;
    setState(() {
      _loadingDevices = true;
      _error = null;
    });
    try {
      final devices = await widget.controller.client.listDevices(session);
      setState(() {
        _devices = devices;
        _loadingDevices = false;
      });
    } catch (e) {
      setState(() {
        _error = '$e';
        _loadingDevices = false;
      });
    }
  }

  Future<void> _logout() async {
    widget.launcher.terminateAll();
    await widget.controller.logout();
    setState(() {
      _devices = [];
      _states.clear();
      _pids.clear();
    });
  }

  Future<void> _connect(CloudDevice device) async {
    final session = widget.controller.session;
    if (session == null) return;
    setState(() => _states[device.id] = SessionState.connecting);
    try {
      final ticket = await widget.controller.client.connect(session, device.id);
      final proc = await widget.launcher.launchCloud(
        ticket,
        title: device.name,
        onState: (s) => setState(() => _states[device.id] = s),
        onExit: (_) => setState(() {
          _states[device.id] = SessionState.disconnected;
          _pids.remove(device.id);
        }),
      );
      setState(() => _pids[device.id] = proc.pid);
    } catch (e) {
      setState(() => _states[device.id] = SessionState.error);
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(content: Text('Failed to connect: $e')),
        );
      }
    }
  }

  void _disconnect(CloudDevice device) {
    final pid = _pids[device.id];
    if (pid != null) widget.launcher.terminate(pid);
  }

  @override
  Widget build(BuildContext context) {
    if (widget.controller.loadingSession) {
      return const Center(child: CircularProgressIndicator());
    }
    final session = widget.controller.session;
    if (session == null) {
      return _LoginForm(controller: widget.controller);
    }
    return _DeviceList(
      session: session,
      devices: _devices,
      loading: _loadingDevices,
      error: _error,
      states: _states,
      pids: _pids,
      onRefresh: _refreshDevices,
      onLogout: _logout,
      onConnect: _connect,
      onDisconnect: _disconnect,
    );
  }
}

class _DeviceList extends StatelessWidget {
  final CloudSession session;
  final List<CloudDevice> devices;
  final bool loading;
  final String? error;
  final Map<String, SessionState> states;
  final Map<String, int> pids;
  final Future<void> Function() onRefresh;
  final Future<void> Function() onLogout;
  final Future<void> Function(CloudDevice) onConnect;
  final void Function(CloudDevice) onDisconnect;

  const _DeviceList({
    required this.session,
    required this.devices,
    required this.loading,
    required this.error,
    required this.states,
    required this.pids,
    required this.onRefresh,
    required this.onLogout,
    required this.onConnect,
    required this.onDisconnect,
  });

  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        Padding(
          padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
          child: Row(
            children: [
              Expanded(
                child: Text(
                  'Signed in as ${session.email}'
                  '${session.role == 'ADMIN' ? '  •  admin' : ''}'
                  '${session.creditSeconds != null ? '  •  credit: ${session.creditSeconds}s' : ''}'
                  '  •  ${session.baseUrl}',
                  style: Theme.of(context).textTheme.bodySmall,
                  overflow: TextOverflow.ellipsis,
                ),
              ),
              IconButton(
                icon: const Icon(Icons.refresh),
                tooltip: 'Refresh device list',
                onPressed: loading ? null : onRefresh,
              ),
              TextButton(onPressed: onLogout, child: const Text('Sign out')),
            ],
          ),
        ),
        if (error != null)
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 16),
            child: Text(error!, style: const TextStyle(color: Colors.redAccent)),
          ),
        if (loading) const LinearProgressIndicator(),
        Expanded(
          child: devices.isEmpty && !loading
              ? const Center(child: Text('No devices visible to this account yet.'))
              : ListView.separated(
                  itemCount: devices.length,
                  separatorBuilder: (_, _) => const Divider(height: 1),
                  itemBuilder: (_, i) => _deviceTile(context, devices[i]),
                ),
        ),
      ],
    );
  }

  Widget _deviceTile(BuildContext context, CloudDevice device) {
    final state = states[device.id] ?? SessionState.idle;
    final connected = pids.containsKey(device.id);
    return ListTile(
      leading: Icon(
        device.online ? Icons.circle : Icons.circle_outlined,
        size: 12,
        color: device.online ? Colors.greenAccent : Colors.grey,
      ),
      title: Text(device.name),
      subtitle: Text(
        '${device.relayDeviceId}  •  role: ${device.role}'
        '${device.groupId != null ? '  •  grouped' : ''}'
        '  •  ${device.online ? 'online' : 'offline'}'
        '  •  ${state.name}',
      ),
      trailing: !connected
          ? FilledButton.icon(
              icon: const Icon(Icons.play_arrow),
              label: const Text('Connect'),
              onPressed: () => onConnect(device),
            )
          : OutlinedButton.icon(
              icon: const Icon(Icons.stop),
              label: const Text('Disconnect'),
              onPressed: () => onDisconnect(device),
            ),
    );
  }
}

class _LoginForm extends StatefulWidget {
  final CloudSessionController controller;
  const _LoginForm({required this.controller});

  @override
  State<_LoginForm> createState() => _LoginFormState();
}

class _LoginFormState extends State<_LoginForm> {
  final _baseUrl = TextEditingController(text: 'http://127.0.0.1:4000');
  final _email = TextEditingController();
  final _password = TextEditingController();
  final _displayName = TextEditingController();
  bool _registering = false;
  bool _submitting = false;
  String? _error;

  Future<void> _submit() async {
    setState(() {
      _submitting = true;
      _error = null;
    });
    try {
      if (_registering) {
        await widget.controller.register(
          _baseUrl.text.trim(),
          _email.text.trim(),
          _password.text,
          _displayName.text.trim().isEmpty ? _email.text.trim() : _displayName.text.trim(),
        );
      } else {
        await widget.controller.login(_baseUrl.text.trim(), _email.text.trim(), _password.text);
      }
    } catch (e) {
      setState(() => _error = '$e');
    } finally {
      if (mounted) setState(() => _submitting = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    return Center(
      child: SizedBox(
        width: 360,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(_registering ? 'Create a Nebula Cloud account' : 'Sign in to Nebula Cloud',
                style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 12),
            TextField(
              controller: _baseUrl,
              decoration: const InputDecoration(labelText: 'Server URL', hintText: 'http://127.0.0.1:4000'),
            ),
            const SizedBox(height: 8),
            TextField(
              controller: _email,
              decoration: const InputDecoration(labelText: 'Email'),
              keyboardType: TextInputType.emailAddress,
            ),
            const SizedBox(height: 8),
            TextField(
              controller: _password,
              decoration: const InputDecoration(labelText: 'Password'),
              obscureText: true,
              onSubmitted: (_) => _submit(),
            ),
            if (_registering) ...[
              const SizedBox(height: 8),
              TextField(
                controller: _displayName,
                decoration: const InputDecoration(labelText: 'Display name (optional)'),
              ),
            ],
            if (_error != null) ...[
              const SizedBox(height: 8),
              Text(_error!, style: const TextStyle(color: Colors.redAccent)),
            ],
            const SizedBox(height: 16),
            FilledButton(
              onPressed: _submitting ? null : _submit,
              child: _submitting
                  ? const SizedBox(width: 18, height: 18, child: CircularProgressIndicator(strokeWidth: 2))
                  : Text(_registering ? 'Create account' : 'Sign in'),
            ),
            TextButton(
              onPressed: _submitting ? null : () => setState(() => _registering = !_registering),
              child: Text(_registering ? 'Have an account? Sign in' : "Don't have an account? Register"),
            ),
          ],
        ),
      ),
    );
  }
}
