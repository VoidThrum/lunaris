import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

// The second step of signing in when the account has two-factor sign-in
// on: a 6-digit code from the authenticator app, or a recovery code
// (server/src/mfa.rs). The bridge keeps the sign-in's mfa_token; this only
// passes the code (bridge.loginMfa) and shows what the server answered.
DialogFrame {
    id: dlg
    width: 400
    closePolicy: Popup.CloseOnEscape
    // Closed without a session (Back, Escape): the waiting sign-in is dropped.
    onClosed: if (!dlg.busy) bridge.cancelMfa()

    property string username: ""
    property bool recovery: false
    property bool busy: false
    property string err: ""
    // The sign-in ran out (or was used): the password has to be entered again.
    signal expired()

    function begin(user) {
        username = user;
        recovery = false;
        busy = false;
        err = "";
        codeField.text = "";
        open();
        codeField.input.forceActiveFocus();
    }

    // Digits only, at most six (a pasted "123 456" too).
    function digits(text) { return String(text).replace(/[^0-9]/g, "").slice(0, 6); }

    function submit() {
        var value = recovery ? codeField.text.trim() : digits(codeField.text);
        if (!recovery && value.length !== 6) { err = I18n.t("Enter the 6-digit code from your app"); return; }
        if (recovery && value.replace(/[^0-9A-Za-z]/g, "").length < 8) { err = I18n.t("Enter one of your recovery codes"); return; }
        err = "";
        busy = true;
        bridge.loginMfa(value, recovery);
    }

    function cancel() {
        bridge.cancelMfa();
        busy = false;
        close();
    }

    // What the server answered to the code.
    function answered(success, errorMsg) {
        busy = false;
        if (success) { close(); return; }
        codeField.text = "";
        if (String(errorMsg).indexOf("This sign-in has expired") === 0) {
            close();
            dlg.expired();
            return;
        }
        err = I18n.t(errorMsg);
        codeField.input.forceActiveFocus();
    }

    Connections {
        target: bridge
        function onLoginResult(success, errorMsg, tok, user, srv) {
            if (dlg.opened && dlg.busy) dlg.answered(success, errorMsg);
        }
    }

    contentItem: ColumnLayout {
        spacing: 12
        Text {
            text: I18n.t("Two-factor sign-in")
            color: Theme.textStrong; font.pixelSize: 16; font.weight: Font.DemiBold
        }
        Text {
            Layout.fillWidth: true
            textFormat: Text.PlainText
            wrapMode: Text.WordWrap
            color: Theme.textMuted; font.pixelSize: 13
            text: dlg.recovery
                ? I18n.t("Enter one of the recovery codes you saved when you turned on two-factor sign-in.")
                : I18n.t("Enter the 6-digit code from your authenticator app for %1.").arg(dlg.username)
        }
        Text {
            text: dlg.recovery ? I18n.t("Recovery code") : I18n.t("Code from the app")
            color: Theme.textMuted; font.pixelSize: 12
        }
        InputField {
            id: codeField
            objectName: "mfaCodeField"
            placeholder: dlg.recovery ? "xxxx-xxxx-xxxx" : "123456"
            fontSize: dlg.recovery ? 14 : 18
            input.inputMethodHints: dlg.recovery ? Qt.ImhNoAutoUppercase | Qt.ImhNoPredictiveText : Qt.ImhDigitsOnly
            input.maximumLength: dlg.recovery ? 32 : 7
            input.enabled: !dlg.busy
            onAccepted: dlg.submit()
        }
        Text {
            objectName: "mfaError"
            textFormat: Text.PlainText
            visible: dlg.err !== ""
            text: dlg.err
            color: Theme.danger; font.pixelSize: 12
            wrapMode: Text.WordWrap
            Layout.fillWidth: true
        }
        Text {
            objectName: "mfaSwitch"
            text: dlg.recovery ? I18n.t("Use a code from the app") : I18n.t("Use a recovery code")
            color: switchMa.containsMouse ? Theme.accentFg : Theme.textMuted
            font.pixelSize: 12
            font.underline: switchMa.containsMouse
            MouseArea {
                id: switchMa
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                enabled: !dlg.busy
                onClicked: {
                    dlg.recovery = !dlg.recovery;
                    dlg.err = "";
                    codeField.text = "";
                    codeField.input.forceActiveFocus();
                }
            }
        }
        RowLayout {
            Layout.fillWidth: true
            Layout.topMargin: 4
            spacing: 8
            Item { Layout.fillWidth: true }
            SecondaryButton { text: I18n.t("Back"); enabled: !dlg.busy; onClicked: dlg.cancel() }
            PrimaryButton {
                objectName: "mfaVerify"
                text: dlg.busy ? I18n.t("Verifying…") : I18n.t("Verify")
                enabled: !dlg.busy
                onClicked: dlg.submit()
            }
        }
    }
}
