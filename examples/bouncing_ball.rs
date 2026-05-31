use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ticked::prelude::*;

fn main() -> AppExit {
    App::new()
        .add_plugins((
            DefaultPlugins,
            TickedPlugin,
            PhysicsPlugins::new(TickedSimulation),
        ))
        .register_ticked_component::<Transform>()
        .register_ticked_component::<LinearVelocity>()
        .register_ticked_component::<AngularVelocity>()
        .register_ticked_component::<Position>()
        .register_ticked_component::<Rotation>()
        .add_systems(Startup, setup)
        .add_systems(Update, (keyboard_controls, update_ui))
        .run()
}

#[derive(Component)]
struct TickUiText;

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut counter: ResMut<TickTrackedEntityCounter>,
) {
    // Camera
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 5.0, 10.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // Light
    commands.spawn((
        DirectionalLight {
            illuminance: 5000.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.8, 0.4, 0.0)),
    ));

    // Ground plane — static rigidbody
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::new(Vec3::Y, Vec2::splat(10.0)))),
        MeshMaterial3d(materials.add(Color::srgb(0.3, 0.5, 0.3))),
        RigidBody::Static,
        Collider::half_space(Vec3::Y),
    ));

    // Bouncing ball — dynamic rigidbody
    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(0.5))),
        MeshMaterial3d(materials.add(Color::srgb(0.8, 0.2, 0.2))),
        Transform::from_xyz(0.0, 5.0, 0.0),
        RigidBody::Dynamic,
        Collider::sphere(0.5),
        Restitution::new(0.8),
        counter.next(),
    ));

    // Second ball for variety
    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(0.3))),
        MeshMaterial3d(materials.add(Color::srgb(0.2, 0.4, 0.9))),
        Transform::from_xyz(1.5, 8.0, 0.5),
        RigidBody::Dynamic,
        Collider::sphere(0.3),
        Restitution::new(0.6),
        counter.next(),
    ));

    // UI
    commands.spawn((
        Text::new("Tick: 0 [PLAYING]"),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(10.0),
            left: Val::Px(10.0),
            ..default()
        },
        TickUiText,
    ));

    commands.spawn((
        Text::new("Space: Play/Pause | A/D: Step Back/Forward | Q/E: Scrub Back/Forward | R: Reset"),
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(10.0),
            left: Val::Px(10.0),
            ..default()
        },
    ));
}

fn keyboard_controls(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    ticks_paused: Option<Res<TicksPaused>>,
    mut step_forward: MessageWriter<StepForward>,
    mut step_backward: MessageWriter<StepBackward>,
    mut reset: MessageWriter<ResetToTick>,
) {
    if keys.just_pressed(KeyCode::Space) {
        if ticks_paused.is_some() {
            commands.remove_resource::<TicksPaused>();
        } else {
            commands.insert_resource(TicksPaused);
        }
    }
    // A/D: step once on press
    if keys.just_pressed(KeyCode::KeyD) {
        commands.insert_resource(TicksPaused);
        step_forward.write(StepForward);
    }
    if keys.just_pressed(KeyCode::KeyA) {
        commands.insert_resource(TicksPaused);
        step_backward.write(StepBackward);
    }
    // Q/E: step continuously while held
    if keys.pressed(KeyCode::KeyE) {
        commands.insert_resource(TicksPaused);
        step_forward.write(StepForward);
    }
    if keys.pressed(KeyCode::KeyQ) {
        commands.insert_resource(TicksPaused);
        step_backward.write(StepBackward);
    }
    if keys.just_pressed(KeyCode::KeyR) {
        commands.insert_resource(TicksPaused);
        reset.write(ResetToTick(0));
    }
}

fn update_ui(
    tick: Res<CurrentTick>,
    ticks_paused: Option<Res<TicksPaused>>,
    mut query: Query<&mut Text, With<TickUiText>>,
) {
    for mut text in &mut query {
        let status = if ticks_paused.is_some() { "PAUSED" } else { "PLAYING" };
        **text = format!("Tick: {} [{}]", tick.0, status);
    }
}
