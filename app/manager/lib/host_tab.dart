import 'package:flutter/material.dart';

import 'cloud_session_controller.dart';
import 'host_config_store.dart';
import 'models.dart';
import 'vda_launcher.dart';

// "Host this Mac" tab: registers THIS Mac as a VDA under the currently
// signed-in Cloud account (see cloud_session_controller.dart — same login as
// the "Cloud" tab, no separate sign-in) and supervises a single nebula_vda
// subprocess, mirroring how CloudTab/SessionLauncher supervise nebula_session
// — see server/nebula_cloud/README.md's "Self-registration & trial credit".
class HostTab extends StatefulWidget {
  final CloudSessionController controller;
  final VdaLauncher launcher;
  const HostTab({super.key, required this.controller, required this.launcher});

  @override
  State<HostTab> createState() => _HostTabState();
}

class _HostTabState extends State<HostTab> {
  final _store = HostConfigStore();
  HostConfig? _config;
  bool _loadingConfig = true;
  SessionState _state = SessionState.idle;
  String? _error;

  @override
  void initState() {
    super.initState();
    widget.controller.addListener(_onControllerChanged);
    _loadConfig();
  }

  @override
  void dispose() {
    widget.controller.removeListener(_onControllerChanged);
    super.dispose();
  }

  void _onControllerChanged() => setState(() {});

  Future<void> _loadConfig() async {
    final config = await _store.load();
    setState(() {
      _config = config;
      _loadingConfig = false;
    });
  }

  Future<void> _register(String name, String enrollmentToken) async {
    final session = widget.controller.session;
    if (session == null) return;
    setState(() => _error = null);
    try {
      final result = await widget.controller.client.selfRegisterDevice(session, name, enrollmentToken);
      final config = HostConfig(
        deviceName: result.deviceName,
        relayHost: result.relayHost,
        relayPort: result.relayPort,
        relayDeviceId: result.relayDeviceId,
        relayToken: result.relayToken,
        psk: result.psk,
      );
      await _store.save(config);
      setState(() => _config = config);
      await _startHosting();
    } catch (e) {
      setState(() => _error = '$e');
    }
  }

  Future<void> _startHosting() async {
    final config = _config;
    if (config == null || widget.launcher.isRunning) return;
    setState(() {
      _state = SessionState.connecting;
      _error = null;
    });
    try {
      await widget.launcher.start(
        config,
        onState: (s) => setState(() => _state = s),
        onExit: (code) => setState(() {
          _state = code == 0 ? SessionState.disconnected : SessionState.error;
        }),
      );
      setState(() {}); // refresh isRunning-dependent UI
    } catch (e) {
      setState(() {
        _state = SessionState.error;
        _error = 'Failed to start hosting: $e';
      });
    }
  }

  void _stopHosting() {
    widget.launcher.stop();
    setState(() {});
  }

  Future<void> _forgetRegistration() async {
    if (widget.launcher.isRunning) widget.launcher.stop();
    await _store.clear();
    setState(() {
      _config = null;
      _state = SessionState.idle;
    });
  }

  @override
  Widget build(BuildContext context) {
    if (widget.controller.loadingSession || _loadingConfig) {
      return const Center(child: CircularProgressIndicator());
    }
    if (widget.controller.session == null) {
      return const Center(
        child: Text('Sign in on the Cloud tab first — hosting uses the same account.'),
      );
    }
    final config = _config;
    if (config == null) {
      return _RegisterForm(onRegister: _register, error: _error);
    }
    return _HostingStatus(
      config: config,
      state: _state,
      running: widget.launcher.isRunning,
      error: _error,
      onStart: _startHosting,
      onStop: _stopHosting,
      onForget: _forgetRegistration,
    );
  }
}

class _RegisterForm extends StatefulWidget {
  final Future<void> Function(String name, String enrollmentToken) onRegister;
  final String? error;
  const _RegisterForm({required this.onRegister, required this.error});

  @override
  State<_RegisterForm> createState() => _RegisterFormState();
}

class _RegisterFormState extends State<_RegisterForm> {
  final _name = TextEditingController(text: 'My Mac');
  final _token = TextEditingController();
  bool _submitting = false;

  Future<void> _submit() async {
    setState(() => _submitting = true);
    try {
      await widget.onRegister(_name.text.trim(), _token.text.trim());
    } finally {
      if (mounted) setState(() => _submitting = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    return Center(
      child: SizedBox(
        width: 380,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text('Host this Mac as a VDA', style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            const Text(
              'Registers this Mac under your Cloud account (above) so it shows up '
              'in every signed-in CWA — no separate device setup needed.',
              style: TextStyle(fontSize: 12, color: Colors.grey),
            ),
            const SizedBox(height: 16),
            TextField(
              controller: _name,
              decoration: const InputDecoration(labelText: 'Device name', hintText: 'My Mac mini'),
            ),
            const SizedBox(height: 8),
            TextField(
              controller: _token,
              decoration: const InputDecoration(
                labelText: 'Enrollment token',
                hintText: 'Given to you by your admin (DEVICE_ENROLLMENT_TOKEN)',
              ),
              obscureText: true,
              onSubmitted: (_) => _submit(),
            ),
            if (widget.error != null) ...[
              const SizedBox(height: 8),
              Text(widget.error!, style: const TextStyle(color: Colors.redAccent)),
            ],
            const SizedBox(height: 16),
            FilledButton(
              onPressed: _submitting ? null : _submit,
              child: _submitting
                  ? const SizedBox(width: 18, height: 18, child: CircularProgressIndicator(strokeWidth: 2))
                  : const Text('Register & Start Hosting'),
            ),
          ],
        ),
      ),
    );
  }
}

class _HostingStatus extends StatelessWidget {
  final HostConfig config;
  final SessionState state;
  final bool running;
  final String? error;
  final Future<void> Function() onStart;
  final void Function() onStop;
  final Future<void> Function() onForget;

  const _HostingStatus({
    required this.config,
    required this.state,
    required this.running,
    required this.error,
    required this.onStart,
    required this.onStop,
    required this.onForget,
  });

  @override
  Widget build(BuildContext context) {
    return Center(
      child: SizedBox(
        width: 420,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(
              running ? Icons.podcasts : Icons.desktop_windows_outlined,
              size: 48,
              color: running ? Colors.greenAccent : Colors.grey,
            ),
            const SizedBox(height: 12),
            Text(config.deviceName, style: Theme.of(context).textTheme.titleMedium),
            Text(
              '${config.relayDeviceId}  •  ${running ? state.name : 'not hosting'}',
              style: Theme.of(context).textTheme.bodySmall,
            ),
            const SizedBox(height: 8),
            const Text(
              'Requires Screen Recording (and Accessibility, for mouse/keyboard control) '
              'permission — grant it in System Settings → Privacy & Security on first run.',
              textAlign: TextAlign.center,
              style: TextStyle(fontSize: 12, color: Colors.grey),
            ),
            if (error != null) ...[
              const SizedBox(height: 8),
              Text(error!, style: const TextStyle(color: Colors.redAccent)),
            ],
            const SizedBox(height: 16),
            Row(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                if (!running)
                  FilledButton.icon(
                    icon: const Icon(Icons.play_arrow),
                    label: const Text('Start hosting'),
                    onPressed: onStart,
                  )
                else
                  OutlinedButton.icon(
                    icon: const Icon(Icons.stop),
                    label: const Text('Stop hosting'),
                    onPressed: onStop,
                  ),
                const SizedBox(width: 12),
                TextButton(
                  onPressed: running ? null : onForget,
                  child: const Text('Forget this registration'),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }
}
