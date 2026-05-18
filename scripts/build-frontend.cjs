const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');

const targetDir = path.join(__dirname, '..', 'aistudio-elrc-maker');
const configFile = path.join(targetDir, 'next.config.ts');
const backupFile = path.join(targetDir, 'next.config.ts.bak');

console.log('Starting frontend build process...');

let originalContent = '';
let hasBackup = false;

try {
  // 1. Read the original next.config.ts
  if (!fs.existsSync(configFile)) {
    throw new Error(`Could not find next.config.ts at ${configFile}`);
  }
  originalContent = fs.readFileSync(configFile, 'utf8');

  // 2. Create physical backup file just in case
  fs.writeFileSync(backupFile, originalContent, 'utf8');
  hasBackup = true;
  console.log('Created backup of next.config.ts');

  // 3. Modify content for static export
  // Replace output: 'standalone' with output: 'export'
  let modifiedContent = originalContent.replace(
    /output:\s*['"]standalone['"]/g,
    "output: 'export'"
  );

  // Replace images config to include unoptimized: true
  if (modifiedContent.includes('images: {')) {
    modifiedContent = modifiedContent.replace(
      /images:\s*\{/,
      'images: {\n    unoptimized: true,'
    );
  } else {
    // If not found, add to config
    modifiedContent = modifiedContent.replace(
      /const nextConfig:\s*NextConfig\s*=\s*\{/,
      "const nextConfig: NextConfig = {\n  images: { unoptimized: true },"
    );
  }

  // Write modified content back to next.config.ts
  fs.writeFileSync(configFile, modifiedContent, 'utf8');
  console.log('Modified next.config.ts for static export');

  // 4. Run Next.js build
  console.log('Running next build...');
  execSync('npm run build', {
    cwd: targetDir,
    stdio: 'inherit',
  });
  console.log('Next.js build completed successfully.');

  // 5. Move built output from submodule/out to parent/dist
  const sourceOut = path.join(targetDir, 'out');
  const destDist = path.join(__dirname, '..', 'dist');

  if (fs.existsSync(sourceOut)) {
    console.log(`Moving build output from ${sourceOut} to ${destDist}...`);
    if (fs.existsSync(destDist)) {
      fs.rmSync(destDist, { recursive: true, force: true });
    }
    fs.renameSync(sourceOut, destDist);
    console.log('Moved build output to dist/ folder successfully.');
  } else {
    console.warn(`Warning: Could not find build output at ${sourceOut}`);
  }

} catch (error) {
  console.error('Build process failed:', error);
  process.exitCode = 1;
} finally {
  // 6. Restore original next.config.ts
  if (hasBackup && fs.existsSync(backupFile)) {
    try {
      const backupContent = fs.readFileSync(backupFile, 'utf8');
      fs.writeFileSync(configFile, backupContent, 'utf8');
      fs.unlinkSync(backupFile);
      console.log('Restored original next.config.ts and cleaned up backup file.');
    } catch (restoreError) {
      console.error('CRITICAL: Failed to restore next.config.ts:', restoreError);
    }
  }
}
