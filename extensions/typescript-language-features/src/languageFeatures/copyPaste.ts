/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import * as vscode from 'vscode';
import { DocumentSelector } from '../configuration/documentSelector';

class PasteEditProvider implements vscode.DocumentPasteEditProvider {
	static readonly importsMimeType = 'application/vnd.code.typescript-language-features.copyPaste.imports-in-file';
	static readonly sourceFilePathMimeType = 'application/vnd.code.typescript-language-features.copyPaste.source-file-path';

	async prepareDocumentPaste(document: vscode.TextDocument, ranges: readonly vscode.Range[], dataTransfer: vscode.DataTransfer, token: vscode.CancellationToken): Promise<void> {
		console.log('prepareDocumentPaste', document, ranges, dataTransfer, token);

		dataTransfer.set(PasteEditProvider.importsMimeType, new vscode.DataTransferItem('import * as vscode from "vscode";'));
		dataTransfer.set(PasteEditProvider.sourceFilePathMimeType, new vscode.DataTransferItem(document.uri.toString()));
	}

	async provideDocumentPasteEdits(
		document: vscode.TextDocument,
		ranges: readonly vscode.Range[],
		dataTransfer: vscode.DataTransfer,
		token: vscode.CancellationToken,
	): Promise<vscode.DocumentPasteEdit | undefined> {
		console.log('provideDocumentPasteEdits', document, ranges, dataTransfer, token);
		const imports = await dataTransfer.get(PasteEditProvider.importsMimeType)?.asString();
		const sourceFilePath = await dataTransfer.get(PasteEditProvider.sourceFilePathMimeType)?.asString();

		if (!imports || !sourceFilePath) {
			return undefined; // Wasn't copied from a TS file that has our extension loaded
		}

		const text = await dataTransfer.get('text/plain')?.asString();
		return new vscode.DocumentPasteEdit(`// override paste from ${sourceFilePath}\n\n${text}`);
	}
}

export function register(selector: DocumentSelector) {
	console.log('registering paste provider');
	return vscode.languages.registerDocumentPasteEditProvider(selector.syntax, new PasteEditProvider(), {
		pasteMimeTypes: [
			PasteEditProvider.importsMimeType,
		]
	});
}
