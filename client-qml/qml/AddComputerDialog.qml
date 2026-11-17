import QtQuick
import QtQuick.Layouts

// "Add computer" and "Edit": the Type first, then that type's form.
//
// "Lunaris" is the form there always was (address, password). Every other
// type is a connector, described by data: { id, name, fields: [{ key,
// label, kind, default, required }] } with kind "text", "port" or "secret".
// Nothing here knows a particular connector.
//
// The dialog checks what was typed and hands it over (lunarisSubmitted /
// sourceSubmitted); the owner saves it, and closes the dialog or sets
// `error`. What was typed is dropped when the dialog closes.
DialogFrame {
    id: dlg
    width: 460

    // The connector types to offer besides Lunaris.
    property var connectors: []
    // Server tab: the computer is added to a server, and a connector type
    // is reached through one of its Lunaris computers. `through` lists
    // them with what each offers: [{ host_id, host_name, connectors }].
    property bool serverMode: false
    property var through: []

    property bool editing: false
    // "lunaris" or a connector's id.
    property string typeId: "lunaris"
    property string throughHost: ""
    property string sourceId: ""
    property string editName: ""
    property string editTypeName: ""
    property string editThroughName: ""
    // Edit: the stored values that may be shown (never a secret).
    property var editValues: ({})
    property string error: ""
    property bool busy: false

    signal lunarisSubmitted(string address, string password, string name)
    // `sourceId` is "" when adding. `values` is { "<field key>": "<text>" };
    // when editing, a field left empty is left out (it keeps its value).
    signal sourceSubmitted(string connectorId, string hostId, string sourceId, string name, var values)

    readonly property bool isLunaris: typeId === "lunaris"
    readonly property var typeOptions: [{ value: "lunaris", label: "Lunaris" }].concat(
        connectors.map(function (c) { return { value: c.id, label: c.name }; }))
    // The Lunaris computers that offer the chosen type.
    readonly property var throughOptions: {
        var options = [];
        for (var i = 0; i < through.length; i++) {
            var offered = through[i].connectors || [];
            for (var k = 0; k < offered.length; k++) {
                if (offered[k].id === dlg.typeId) {
                    options.push({ value: through[i].host_id, label: through[i].host_name });
                    break;
                }
            }
        }
        return options;
    }
    // The chosen type's description: as the chosen Lunaris computer has it
    // (computers may run different versions), else as listed.
    readonly property var connector: {
        if (dlg.isLunaris) return null;
        for (var i = 0; i < through.length; i++) {
            if (through[i].host_id !== dlg.throughHost) continue;
            var offered = through[i].connectors || [];
            for (var k = 0; k < offered.length; k++)
                if (offered[k].id === dlg.typeId) return offered[k];
        }
        for (var j = 0; j < connectors.length; j++)
            if (connectors[j].id === dlg.typeId) return connectors[j];
        return null;
    }
    onThroughOptionsChanged: pickThrough()
    function pickThrough() {
        if (!serverMode || editing) return;
        for (var i = 0; i < throughOptions.length; i++)
            if (throughOptions[i].value === throughHost) return;
        throughHost = throughOptions.length > 0 ? throughOptions[0].value : "";
    }

    // A field that may stay empty in the form: on the server every field
    // (its values are never sent back), in the app the secrets.
    function keepsEmpty(field) {
        return dlg.editing && (dlg.serverMode || field.kind === "secret");
    }

    function clearForm() {
        error = "";
        busy = false;
        addressField.text = "";
        lunarisPasswordField.text = "";
        lunarisNameField.text = "";
        lunarisPasswordField.shown = false;
    }

    function openAdd() {
        clearForm();
        editing = false;
        sourceId = "";
        editName = "";
        editValues = ({});
        typeId = "lunaris";
        nameField.text = "";
        open();
        addressField.input.forceActiveFocus();
    }

    // `source`: { id, name, connector, connector_name, host_id (server) };
    // `values`: what may be shown of its fields.
    function openEdit(source, values) {
        clearForm();
        editing = true;
        sourceId = source.id;
        editName = source.name || "";
        editTypeName = source.connector_name || source.connector || "";
        editValues = values || ({});
        throughHost = source.host_id || "";
        editThroughName = source.host_name || "";
        typeId = source.connector || "";
        nameField.text = editName;
        open();
        nameField.input.forceActiveFocus();
    }

    function submit() {
        if (busy) return;
        error = "";
        if (isLunaris) {
            if (addressField.text.trim() === "") { error = I18n.t("Enter the address of the computer"); return; }
            lunarisSubmitted(addressField.text.trim(), lunarisPasswordField.text.trim(), lunarisNameField.text.trim());
            return;
        }
        if (serverMode && throughHost === "") { error = I18n.t("Choose the Lunaris computer it is reached through"); return; }
        var name = nameField.text.trim();
        if (name === "") { error = I18n.t("Give the computer a name"); return; }
        var values = {};
        for (var i = 0; i < fieldRepeater.count; i++) {
            var item = fieldRepeater.itemAt(i);
            if (!item) continue;
            var field = item.field;
            // A password is taken as typed; anything else without the spaces around it.
            var text = field.kind === "secret" ? item.value : item.value.trim();
            if (text === "") {
                if (keepsEmpty(field)) continue;
                if (field.required) { error = I18n.t("%1 is required").arg(I18n.t(field.label)); return; }
                if (dlg.editing) continue;
            } else if (field.kind === "port" && !(/^[0-9]{1,5}$/.test(text) && parseInt(text) >= 1 && parseInt(text) <= 65535)) {
                error = I18n.t("%1 must be a port from 1 to 65535").arg(I18n.t(field.label));
                return;
            }
            values[field.key] = text;
        }
        sourceSubmitted(typeId, throughHost, sourceId, name, values);
    }

    onClosed: {
        clearForm();
        nameField.text = "";
    }

    IntValidator { id: portValidator; bottom: 1; top: 65535 }

    component FieldLabel: Text {
        Layout.fillWidth: true
        color: Theme.textMuted
        font.pixelSize: 12
        font.weight: Font.Medium
        elide: Text.ElideRight
    }
    component Note: Text {
        Layout.fillWidth: true
        color: Theme.textMuted
        font.pixelSize: 11
        wrapMode: Text.WordWrap
    }

    contentItem: ColumnLayout {
        spacing: 12

        Text {
            Layout.fillWidth: true
            text: dlg.editing ? I18n.t("Edit %1").arg(dlg.editName) : I18n.t("Add computer")
            color: Theme.textStrong
            font.pixelSize: 16
            font.weight: Font.DemiBold
            elide: Text.ElideRight
        }

        // ---- Type ----
        ColumnLayout {
            Layout.fillWidth: true
            spacing: 5
            FieldLabel { text: I18n.t("Type") }
            ThemedCombo {
                objectName: "typeCombo"
                visible: !dlg.editing
                Layout.fillWidth: true
                options: dlg.typeOptions
                value: dlg.typeId
                onPicked: (v) => { dlg.error = ""; dlg.typeId = v; }
                Accessible.name: I18n.t("Type")
            }
            // The type of a computer stays what it was added as.
            Text {
                visible: dlg.editing
                Layout.fillWidth: true
                text: I18n.t(dlg.editTypeName)
                color: Theme.textStrong
                font.pixelSize: 13
                elide: Text.ElideRight
            }
        }

        // ---- Lunaris ----
        ColumnLayout {
            visible: dlg.isLunaris
            Layout.fillWidth: true
            spacing: 12
            Note {
                font.pixelSize: 12
                text: dlg.serverMode
                    ? I18n.t("The server connects to the Lunaris app on that computer (remote control turned on). Its address and password are shown under \"This computer\" there.")
                    : I18n.t("A computer with the Lunaris app (remote control turned on). Its address and password are shown under \"This computer\" there.")
            }
            ColumnLayout {
                Layout.fillWidth: true
                spacing: 5
                FieldLabel { text: I18n.t("Address") + " *" }
                InputField { id: addressField; objectName: "addressField"; placeholder: I18n.t("Address, e.g. 192.168.1.20"); onAccepted: dlg.submit() }
            }
            ColumnLayout {
                Layout.fillWidth: true
                spacing: 5
                FieldLabel { text: I18n.t("Password (optional)") }
                RowLayout {
                    Layout.fillWidth: true
                    spacing: 4
                    InputField {
                        id: lunarisPasswordField
                        objectName: "lunarisPasswordField"
                        property bool shown: false
                        password: !shown
                        onAccepted: dlg.submit()
                    }
                    IconButton {
                        iconName: lunarisPasswordField.shown ? "eyeOff" : "eye"
                        tip: lunarisPasswordField.shown ? I18n.t("Hide password") : I18n.t("Show password")
                        onClicked: lunarisPasswordField.shown = !lunarisPasswordField.shown
                    }
                }
                Note { text: I18n.t("Without the password, the person at that computer is asked to accept each connection.") }
            }
            ColumnLayout {
                visible: !dlg.serverMode
                Layout.fillWidth: true
                spacing: 5
                FieldLabel { text: I18n.t("Name (optional)") }
                InputField { id: lunarisNameField; objectName: "lunarisNameField"; placeholder: I18n.t("Name, e.g. Office PC"); input.maximumLength: 64; onAccepted: dlg.submit() }
            }
        }

        // ---- A connector type ----
        ColumnLayout {
            visible: !dlg.isLunaris
            Layout.fillWidth: true
            spacing: 12

            ColumnLayout {
                visible: dlg.serverMode
                Layout.fillWidth: true
                spacing: 5
                FieldLabel { text: I18n.t("Through which Lunaris computer") }
                ThemedCombo {
                    visible: !dlg.editing
                    Layout.fillWidth: true
                    options: dlg.throughOptions
                    value: dlg.throughHost
                    onPicked: (v) => dlg.throughHost = v
                    Accessible.name: I18n.t("Through which Lunaris computer")
                }
                Text {
                    visible: dlg.editing
                    Layout.fillWidth: true
                    text: {
                        for (var i = 0; i < dlg.through.length; i++)
                            if (dlg.through[i].host_id === dlg.throughHost) return dlg.through[i].host_name;
                        return dlg.editThroughName;
                    }
                    color: Theme.textStrong
                    font.pixelSize: 13
                    elide: Text.ElideRight
                }
                Note {
                    visible: !dlg.editing
                    text: I18n.t("That computer connects to the new one and shows it. The address and password are kept there, not on the server.")
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 5
                FieldLabel { text: I18n.t("Name") + " *" }
                InputField { id: nameField; objectName: "nameField"; placeholder: I18n.t("Name, e.g. Office PC"); input.maximumLength: 64; onAccepted: dlg.submit() }
            }

            Repeater {
                id: fieldRepeater
                // Only while the dialog is open, so that nothing typed outlives it.
                model: dlg.visible && dlg.connector ? dlg.connector.fields : []
                delegate: ColumnLayout {
                    id: fieldItem
                    required property var modelData
                    readonly property var field: modelData
                    readonly property bool secret: modelData.kind === "secret"
                    property alias value: fieldInput.text
                    property bool shown: false
                    Layout.fillWidth: true
                    spacing: 5
                    FieldLabel {
                        text: I18n.t(fieldItem.field.label) + (fieldItem.field.required && !dlg.keepsEmpty(fieldItem.field) ? " *" : "")
                    }
                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 4
                        InputField {
                            id: fieldInput
                            objectName: "field-" + fieldItem.field.key
                            Layout.fillWidth: fieldItem.field.kind !== "port"
                            implicitWidth: fieldItem.field.kind === "port" ? 110 : 200
                            password: fieldItem.secret && !fieldItem.shown
                            // Edit: what is stored and may be shown; add: the connector's default.
                            text: dlg.editing ? String(dlg.editValues[fieldItem.field.key] || "")
                                              : String(fieldItem.field["default"] || "")
                            // (A port's box is too narrow for a hint; the note below says it.)
                            placeholder: !dlg.editing || fieldItem.field.kind === "port" ? ""
                                : fieldItem.secret ? I18n.t("Leave empty to keep the password")
                                : dlg.serverMode ? I18n.t("Leave empty to keep it") : ""
                            input.validator: fieldItem.field.kind === "port" ? portValidator : null
                            input.maximumLength: fieldItem.field.kind === "port" ? 5 : 255
                            input.inputMethodHints: fieldItem.secret ? Qt.ImhSensitiveData | Qt.ImhNoPredictiveText : Qt.ImhNone
                            onAccepted: dlg.submit()
                            Accessible.name: I18n.t(fieldItem.field.label)
                        }
                        IconButton {
                            visible: fieldItem.secret
                            iconName: fieldItem.shown ? "eyeOff" : "eye"
                            tip: fieldItem.shown ? I18n.t("Hide password") : I18n.t("Show password")
                            onClicked: fieldItem.shown = !fieldItem.shown
                        }
                        Item { visible: fieldItem.field.kind === "port"; Layout.fillWidth: true }
                    }
                }
            }

            Note {
                visible: dlg.editing && dlg.serverMode && dlg.connector !== null
                text: I18n.t("Leave a field empty to keep it. The server never shows what is stored.")
            }
            Note {
                visible: dlg.editing && dlg.connector === null
                text: dlg.serverMode
                    ? I18n.t("Its other fields can be changed when the Lunaris computer it is reached through is online.")
                    : I18n.t("This type is not installed any more: only the name can be changed.")
            }
        }

        Text {
            visible: dlg.error !== ""
            Layout.fillWidth: true
            text: dlg.error
            color: Theme.danger
            font.pixelSize: 12
            wrapMode: Text.WordWrap
        }

        RowLayout {
            Layout.fillWidth: true
            spacing: 8
            Item { Layout.fillWidth: true }
            SecondaryButton { text: I18n.t("Cancel"); onClicked: dlg.close() }
            PrimaryButton {
                objectName: "submitButton"
                text: dlg.editing ? (dlg.busy ? I18n.t("Saving…") : I18n.t("Save"))
                                  : (dlg.busy ? I18n.t("Adding…") : I18n.t("Add"))
                onClicked: dlg.submit()
            }
        }
    }
}
