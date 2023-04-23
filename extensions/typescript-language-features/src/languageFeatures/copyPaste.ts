/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import * as vscode from 'vscode';
import { DocumentSelector } from '../configuration/documentSelector';

class PasteEditProvider implements vscode.DocumentPasteEditProvider {
	async prepareDocumentPaste(document: vscode.TextDocument, ranges: readonly vscode.Range[], dataTransfer: vscode.DataTransfer, token: vscode.CancellationToken): Promise<void> {
		console.log('prepareDocumentPaste', document, ranges, dataTransfer, token);
		return undefined;
	}

	async provideDocumentPasteEdits(
		document: vscode.TextDocument,
		ranges: readonly vscode.Range[],
		dataTransfer: vscode.DataTransfer,
		token: vscode.CancellationToken,
	): Promise<vscode.DocumentPasteEdit | undefined> {
		console.log('provideDocumentPasteEdits', document, ranges, dataTransfer, token);
		return undefined;
	}
}

export function register(selector: DocumentSelector) {
	console.log('registering paste provider');
	return vscode.languages.registerDocumentPasteEditProvider(selector.syntax, new PasteEditProvider(), {
		pasteMimeTypes: [
			'*/*' // Try all for now
		]
	});
}
