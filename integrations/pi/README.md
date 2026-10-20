Test: `archive.test.mts` imports pi's packages, so link them first:

    ln -sfn <pi install>/node_modules/@earendil-works node_modules/@earendil-works && node archive.test.mts
