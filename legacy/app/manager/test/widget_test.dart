import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:nebula_manager/main.dart';

void main() {
  testWidgets('shows the Nebula connection manager', (WidgetTester tester) async {
    await tester.pumpWidget(const NebulaManagerApp());
    await tester.pumpAndSettle();

    expect(find.text('Nebula — Connections'), findsOneWidget);
    expect(find.text('No VDAs yet. Tap + to add one.'), findsOneWidget);
  });

  testWidgets('the Cloud tab shows a sign-in form when logged out', (WidgetTester tester) async {
    await tester.pumpWidget(const NebulaManagerApp());
    await tester.pumpAndSettle();

    // Switch from the default "Direct" tab to "Cloud".
    await tester.tap(find.widgetWithText(Tab, 'Cloud'));
    await tester.pumpAndSettle();

    expect(find.text('Sign in to Nebula Cloud'), findsOneWidget);
    expect(find.widgetWithText(TextField, 'Server URL'), findsOneWidget);
    expect(find.widgetWithText(TextField, 'Email'), findsOneWidget);
    expect(find.widgetWithText(TextField, 'Password'), findsOneWidget);
    // The "Add VDA" action is Direct-tab-only.
    expect(find.byTooltip('Add VDA'), findsNothing);

    // Switching between the login and registration forms toggles the CTA copy.
    await tester.tap(find.text("Don't have an account? Register"));
    await tester.pumpAndSettle();
    expect(find.text('Create a Nebula Cloud account'), findsOneWidget);
    expect(find.widgetWithText(FilledButton, 'Create account'), findsOneWidget);
  });

  testWidgets('the Host this Mac tab asks to sign in first when logged out', (WidgetTester tester) async {
    await tester.pumpWidget(const NebulaManagerApp());
    await tester.pumpAndSettle();

    await tester.tap(find.widgetWithText(Tab, 'Host this Mac'));
    await tester.pumpAndSettle();

    expect(
      find.text('Sign in on the Cloud tab first — hosting uses the same account.'),
      findsOneWidget,
    );
    // No registration form should be visible until signed in.
    expect(find.text('Host this Mac as a VDA'), findsNothing);
  });
}
