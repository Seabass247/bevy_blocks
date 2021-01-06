use std::collections::{HashMap, HashSet};

use super::{
    constants::*,
    save_load::{load_chunk_from_file, save_chunk_to_file},
};
use building_blocks::{core::prelude::*, storage::ChunkHashMap};
use building_blocks::{
    mesh::{greedy_quads::*, MaterialVoxel, PosNormTexMesh},
    storage::{prelude::*, IsEmpty},
};
use noise::{MultiFractal, NoiseFn, RidgedMulti, Seedable};
use rand::Rng;
use serde::{Deserialize, Serialize};

use bevy::{
    asset::LoadState,
    prelude::*,
    reflect::TypeUuid,
    render::{
        mesh::{Indices, VertexAttributeValues},
        pipeline::{PipelineDescriptor, PrimitiveTopology, RenderPipeline},
        render_graph::{base, AssetRenderResourcesNode, RenderGraph},
        renderer::RenderResources,
        shader::{ShaderDefs, ShaderStages},
        texture::AddressMode,
    },
    tasks::{ComputeTaskPool, TaskPool},
};

use bevy_rapier3d::{
    physics::{ColliderHandleComponent, RigidBodyHandleComponent},
    rapier::{
        dynamics::{JointSet, RigidBodyBuilder, RigidBodyHandle, RigidBodySet},
        geometry::{ColliderBuilder, ColliderSet},
    },
};

const STAGE: &str = "plugin_state";

#[derive(Clone)]
enum PluginState {
    PreInit,
    Init,
    Finished,
}

pub struct VoxelTerrainGeneratorPlugin;

impl Plugin for VoxelTerrainGeneratorPlugin {
    fn build(&self, builder: &mut AppBuilder) {
        builder
            .add_asset::<TerrainMaterial>()
            .add_resource(State::new(PluginState::PreInit))
            .add_resource(MeshGeneratorState::new())
            .add_resource::<GeneratedVoxelResource>(GeneratedVoxelResource::default())
            .add_resource::<GeneratedMeshesResource>(GeneratedMeshesResource::default())
            .init_resource::<VoxelAssetHandles>()
            .add_stage_after(stage::UPDATE, STAGE, StateStage::<PluginState>::default())
            .on_state_enter(STAGE, PluginState::PreInit, load_assets.system())
            .on_state_update(STAGE, PluginState::PreInit, check_assets.system())
            .on_state_enter(STAGE, PluginState::Init, setup_generator_system.system())
            .on_state_update(
                STAGE,
                PluginState::Finished,
                generate_chunks_system.system(),
            )
            .on_state_update(
                STAGE,
                PluginState::Finished,
                generate_chunk_meshes_system.system(),
            );
        //.on_state_enter(STAGE, PluginState::Finished, voxel_generator_system.system())
    }
}

fn load_assets(mut handles: ResMut<VoxelAssetHandles>, asset_server: Res<AssetServer>) {
    const TERRAIN_TEXTURE_PATH: &str = "../assets/textures/terrain.png";
    const FRAGMENT_SHADER_PATH: &str = "../assets/shaders/voxel.frag";
    const VERTEX_SHADER_PATH: &str = "../assets/shaders/voxel.vert";
    
    // Enable hot asset reloading
    asset_server.watch_for_changes().unwrap();

    let texture: Handle<Texture> = asset_server.load(TERRAIN_TEXTURE_PATH);
    handles.vec.push(texture.clone_untyped());
    handles.texture = texture;

    let vert_shader = asset_server.load::<Shader, _>(VERTEX_SHADER_PATH);
    handles.vec.push(vert_shader.clone_untyped());
    handles.vert_shader = vert_shader;

    let frag_shader = asset_server.load::<Shader, _>(FRAGMENT_SHADER_PATH);
    handles.vec.push(frag_shader.clone_untyped());
    handles.frag_shader = frag_shader;
}

fn check_assets(
    mut state: ResMut<State<PluginState>>,
    handles: ResMut<VoxelAssetHandles>,
    asset_server: Res<AssetServer>,
) {
    if let LoadState::Loaded =
        asset_server.get_group_load_state(handles.vec.iter().map(|handle| handle.id))
    {
        state.set_next(PluginState::Init).unwrap();
    }
}

pub struct MeshGeneratorState {
    chunk_mesh_entities: Vec<Entity>,
}

impl MeshGeneratorState {
    pub fn new() -> Self {
        Self {
            chunk_mesh_entities: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct VoxelAssetHandles {
    vert_shader: Handle<Shader>,
    frag_shader: Handle<Shader>,
    texture: Handle<Texture>,
    material: Handle<TerrainMaterial>,
    pipeline: Handle<PipelineDescriptor>,
    vec: Vec<HandleUntyped>,
}

pub type VoxelMap = ChunkHashMap<[i32; 3], Voxel, ()>;

struct GeneratedVoxelResource {
    pub noise: RidgedMulti,
    pub chunk_size: i32,
    pub map: VoxelMap,
    pub max_height: i32,
    pub view_distance: i32,
    pub materials: Vec<Handle<StandardMaterial>>,
}

impl Default for GeneratedVoxelResource {
    fn default() -> Self {
        let builder = ChunkMapBuilder {
            chunk_shape: PointN([CHUNK_SIZE; 3]),
            ambient_value: Voxel(0),
            default_chunk_metadata: (),
        };

        GeneratedVoxelResource {
            noise: RidgedMulti::new()
                .set_seed(1234)
                .set_frequency(0.008)
                .set_octaves(5),
            chunk_size: CHUNK_SIZE,
            map: builder.build_with_hash_map_storage(),
            max_height: 256,
            view_distance: 256,
            materials: Vec::new(),
        }
    }
}

type VoxelType = u8;
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Voxel(pub VoxelType);

impl Default for Voxel {
    fn default() -> Self {
        Voxel(0)
    }
}

impl IsEmpty for Voxel {
    fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

impl MaterialVoxel for Voxel {
    type Material = VoxelType;

    fn material(&self) -> Self::Material {
        self.0
    }
}

#[derive(RenderResources, ShaderDefs, Default, TypeUuid)]
#[uuid = "620f651b-adbe-464b-b740-ba0e547282ba"]
pub struct TerrainMaterial {
    pub albedo: Color,
    pub albedo_texture: Option<Handle<Texture>>,
    pub custom_val: f32,
    #[render_resources(ignore)]
    pub shaded: bool,
}

fn setup_generator_system(
    mut state: ResMut<State<PluginState>>,
    asset_server: ResMut<AssetServer>,
    mut pipelines: ResMut<Assets<PipelineDescriptor>>,
    mut materials: ResMut<Assets<TerrainMaterial>>,
    mut textures: ResMut<Assets<Texture>>,
    mut render_graph: ResMut<RenderGraph>,
    mut handles: ResMut<VoxelAssetHandles>,
) {
    // Create a new shader pipeline
    let pipeline_handle = pipelines.add(PipelineDescriptor::default_config(ShaderStages {
        vertex: handles.vert_shader.clone(),
        fragment: Some(handles.frag_shader.clone()),
    }));

    // Add an AssetRenderResourcesNode to our Render Graph. This will bind TerrainMaterial resources to our shader
    render_graph.add_system_node(
        "terrain_material",
        AssetRenderResourcesNode::<TerrainMaterial>::new(true),
    );

    // Add a Render Graph edge connecting our new "voxel_material" node to the main pass node. This ensures "voxel_material" runs before the main pass
    render_graph
        .add_node_edge("terrain_material", base::node::MAIN_PASS)
        .unwrap();

    // Create a new material
    let material_handle = materials.add(TerrainMaterial {
        albedo: Color::rgb(1.0, 1.0, 1.0),
        albedo_texture: Some(handles.texture.clone()),
        custom_val: 0.0,
        shaded: true,
    });

    let texture = textures.get_mut(handles.texture.clone()).unwrap();

    texture.sampler.address_mode_u = AddressMode::Repeat;
    texture.sampler.address_mode_v = AddressMode::Repeat;
    texture.sampler.address_mode_w = AddressMode::Repeat;

    // Create a new array texture asset from the loaded texture.
    let array_layers = NUM_TEXTURE_LAYERS + 1;
    texture.reinterpret_stacked_2d_as_array(array_layers);

    handles.material = material_handle;
    handles.pipeline = pipeline_handle;

    state.set_next(PluginState::Finished).unwrap();
}

pub fn voxel_generator_system(
    commands: &mut Commands,
    mut assets: ResMut<VoxelAssetHandles>,
    mut textures: ResMut<Assets<Texture>>,
    pool: Res<ComputeTaskPool>,
    mut state: ResMut<MeshGeneratorState>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut voxel_materials: ResMut<Assets<TerrainMaterial>>,
) {
    let new_shape_requested = false;

    if new_shape_requested || state.chunk_mesh_entities.is_empty() {
        // Delete the old meshes.
        for entity in state.chunk_mesh_entities.drain(..) {
            commands.despawn(entity);
        }

        let voxel_material_handle = assets.material.clone();

        let render_pipelines =
            RenderPipelines::from_pipelines(vec![RenderPipeline::new(assets.pipeline.clone())]);

        // Sample the new shape.
        let chunk_meshes = generate_chunk_meshes(Terrain::AllBlocks, &pool.0);
        for mesh in chunk_meshes.into_iter() {
            if let Some(mesh) = mesh {
                if mesh.pos_norm_tex_mesh.is_empty() {
                    continue;
                }

                state.chunk_mesh_entities.push(create_mesh_entity(
                    mesh,
                    commands,
                    voxel_material_handle.clone(),
                    render_pipelines.clone(),
                    &mut meshes,
                ));
            }
        }
    }
}

fn create_mesh_entity(
    mesh_data: ChunkMeshData,
    commands: &mut Commands,
    voxel_material: Handle<TerrainMaterial>,
    pipelines: RenderPipelines,
    meshes: &mut Assets<Mesh>,
) -> Entity {
    let mesh = mesh_data.pos_norm_tex_mesh;

    assert_eq!(mesh.positions.len(), mesh.normals.len());

    let _num_vertices = mesh.positions.len();

    let mut render_mesh = Mesh::new(PrimitiveTopology::TriangleList);
    render_mesh.set_attribute(
        Mesh::ATTRIBUTE_POSITION,
        VertexAttributeValues::Float3(mesh.positions),
    );
    render_mesh.set_attribute(
        Mesh::ATTRIBUTE_NORMAL,
        VertexAttributeValues::Float3(mesh.normals),
    );
    render_mesh.set_attribute(
        Mesh::ATTRIBUTE_UV_0,
        VertexAttributeValues::Float2(mesh.tex_coords),
    );
    render_mesh.set_attribute(
        "Vertex_Voxel_Material",
        VertexAttributeValues::Float(mesh_data.vert_vox_mat_vals),
    );

    render_mesh.set_attribute(
        "Vertex_AO",
        VertexAttributeValues::Float(mesh_data.vert_ao_vals),
    );

    render_mesh.set_indices(Some(Indices::U32(
        mesh.indices.iter().map(|i| *i as u32).collect(),
    )));

    commands
        .spawn(MeshBundle {
            mesh: meshes.add(render_mesh),
            render_pipelines: pipelines,
            transform: Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)),
            ..Default::default()
        })
        .with(voxel_material)
        .current_entity()
        .unwrap()
}

struct ChunkMeshData {
    pos_norm_tex_mesh: PosNormTexMesh,
    vert_vox_mat_vals: Vec<f32>,
    vert_ao_vals: Vec<f32>,
}

fn generate_chunk_meshes(voxel_generation: Terrain, pool: &TaskPool) -> Vec<Option<ChunkMeshData>> {
    let voxels = voxel_generation.get_voxels();

    // Chunk up the voxels just to show that meshing across chunks is consistent.
    let chunk_shape = PointN([CHUNK_SIZE; 3]);
    let ambient_value = Voxel(0);

    let builder = ChunkMapBuilder {
        chunk_shape,
        ambient_value,
        default_chunk_metadata: (),
    };
    // Normally we'd keep this map around in a resource, but we don't need to for this specific example. We could also use an
    // Array3 here instead of a ChunkMap3, but we use chunks for educational purposes.
    let mut map = builder.build_with_hash_map_storage();
    copy_extent(voxels.extent(), &voxels, &mut map);

    // Generate the chunk meshes.
    let map_ref = &map;

    pool.scope(|s| {
        for chunk_key in map_ref.storage().keys() {
            s.spawn(async move {
                let padded_chunk_extent = padded_greedy_quads_chunk_extent(
                    &map_ref.indexer.extent_for_chunk_at_key(*chunk_key),
                );

                let mut padded_chunk = Array3::fill(padded_chunk_extent, Voxel(0));
                copy_extent(&padded_chunk_extent, map_ref, &mut padded_chunk);

                // TODO bevy: we could avRoid re-allocating the buffers on every call if we had
                // thread-local storage accessible from this task
                let mut buffer = GreedyQuadsBuffer::new(padded_chunk_extent);
                greedy_quads(&padded_chunk, &padded_chunk_extent, &mut buffer);

                let mut vert_vox_mat_vals: Vec<f32> = Vec::new();
                let mut vert_ao_vals: Vec<f32> = Vec::new();

                let mut mesh = PosNormTexMesh::default();
                for group in buffer.quad_groups.iter() {
                    for (quad, material) in group.quads.iter() {
                        for v in group.face.quad_corners(quad).iter() {
                            let v_ao =
                                get_ao_at_vert(*v, &padded_chunk, &padded_chunk_extent) as f32;
                            vert_ao_vals.extend_from_slice(&[v_ao]);
                        }

                        group.face.add_quad_to_pos_norm_tex_mesh(&quad, &mut mesh);
                        let voxel_mat = *material as f32;
                        vert_vox_mat_vals
                            .extend_from_slice(&[voxel_mat, voxel_mat, voxel_mat, voxel_mat]);
                    }
                }

                if mesh.is_empty() {
                    None
                } else {
                    Some(ChunkMeshData {
                        pos_norm_tex_mesh: mesh,
                        vert_vox_mat_vals,
                        vert_ao_vals,
                    })
                }
            })
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Terrain {
    Natural,
    AllBlocks,
    Debug,
}

impl Terrain {
    fn get_voxels(&self) -> Array3<Voxel> {
        match self {
            Terrain::Natural => {
                let mut rng = rand::thread_rng();
                let rand_seed: u32 = rng.gen();
                let noise = RidgedMulti::new()
                    .set_seed(rand_seed)
                    .set_frequency(0.008)
                    .set_octaves(5);
                let yoffset = SEA_LEVEL;
                let yscale = TERRAIN_Y_SCALE * yoffset;

                let extent =
                    Extent3i::from_min_and_shape(PointN([-20; 3]), PointN([40; 3])).padded(1);
                let mut voxels = Array3::fill(extent, Voxel(0));
                for z in 0..40 {
                    for x in 0..40 {
                        let max_y =
                            (noise.get([x as f64, z as f64]) * yscale + yoffset).round() as i32;
                        let level =
                            Extent3i::from_min_and_shape(PointN([x, 0, z]), PointN([1, max_y, 1]));
                        let vox_material = rng.gen_range(1, 5) as VoxelType;
                        voxels.fill_extent(&level, Voxel(vox_material));
                    }
                }

                voxels
            }
            Terrain::AllBlocks => {
                let extent =
                    Extent3i::from_min_and_shape(PointN([-20; 3]), PointN([40; 3])).padded(1);
                let mut voxels = Array3::fill(extent, Voxel(0));

                let debug_blocks_0 =
                    Extent3i::from_min_and_shape(PointN([1, 1, 1]), PointN([1, 1, 1]));
                let debug_blocks_1 =
                    Extent3i::from_min_and_shape(PointN([2, 2, 2]), PointN([1, 1, 1]));
                let debug_blocks_2 =
                    Extent3i::from_min_and_shape(PointN([3, 3, 3]), PointN([1, 1, 1]));
                let debug_blocks_3 =
                    Extent3i::from_min_and_shape(PointN([4, 4, 4]), PointN([1, 1, 1]));

                voxels.fill_extent(&debug_blocks_0, Voxel(1));
                voxels.fill_extent(&debug_blocks_1, Voxel(2));
                voxels.fill_extent(&debug_blocks_2, Voxel(3));
                voxels.fill_extent(&debug_blocks_3, Voxel(4));

                voxels
            }
            Terrain::Debug => {
                let extent =
                    Extent3i::from_min_and_shape(PointN([-20; 3]), PointN([40; 3])).padded(1);
                let mut voxels = Array3::fill(extent, Voxel(0));

                let debug_blocks_0 =
                    Extent3i::from_min_and_shape(PointN([5, 2, 5]), PointN([1, 1, 1]));
                let debug_blocks_1 =
                    Extent3i::from_min_and_shape(PointN([7, 2, 5]), PointN([1, 1, 1]));
                let debug_blocks_2 =
                    Extent3i::from_min_and_shape(PointN([7, 3, 6]), PointN([1, 1, 1]));
                let debug_blocks_3 =
                    Extent3i::from_min_and_shape(PointN([9, 2, 5]), PointN([1, 1, 1]));
                let debug_blocks_4 =
                    Extent3i::from_min_and_shape(PointN([9, 3, 6]), PointN([1, 1, 1]));
                let debug_blocks_5 =
                    Extent3i::from_min_and_shape(PointN([10, 3, 6]), PointN([1, 1, 1]));
                let debug_blocks_6 =
                    Extent3i::from_min_and_shape(PointN([12, 2, 5]), PointN([1, 1, 1]));
                let debug_blocks_7 =
                    Extent3i::from_min_and_shape(PointN([12, 3, 6]), PointN([1, 1, 1]));
                let debug_blocks_8 =
                    Extent3i::from_min_and_shape(PointN([13, 3, 6]), PointN([1, 1, 1]));
                let debug_blocks_9 =
                    Extent3i::from_min_and_shape(PointN([13, 3, 5]), PointN([1, 1, 1]));
                voxels.fill_extent(&debug_blocks_0, Voxel(1));
                voxels.fill_extent(&debug_blocks_1, Voxel(1));
                voxels.fill_extent(&debug_blocks_2, Voxel(1));
                voxels.fill_extent(&debug_blocks_3, Voxel(1));
                voxels.fill_extent(&debug_blocks_4, Voxel(1));
                voxels.fill_extent(&debug_blocks_5, Voxel(1));
                voxels.fill_extent(&debug_blocks_6, Voxel(1));
                voxels.fill_extent(&debug_blocks_7, Voxel(1));
                voxels.fill_extent(&debug_blocks_8, Voxel(1));
                voxels.fill_extent(&debug_blocks_9, Voxel(1));
                voxels
            }
        }
    }
}

fn get_ao_at_vert(
    v: Point3f,
    padded_chunk: &ArrayN<[i32; 3], Voxel>,
    padded_chunk_extent: &Extent3i,
) -> i32 {
    let loc: Point3i = PointN([(v.x()) as i32, (v.y()) as i32, (v.z()) as i32]);

    let top0_loc = PointN([loc.x() - 1, loc.y(), loc.z()]);
    let top1_loc: Point3i = PointN([loc.x(), loc.y(), loc.z() - 1]);
    let top2_loc: Point3i = PointN([loc.x(), loc.y(), loc.z()]);
    let top3_loc: Point3i = PointN([loc.x() - 1, loc.y(), loc.z() - 1]);

    let bot0_loc: Point3i = PointN([loc.x() - 1, loc.y() - 1, loc.z()]);
    let bot1_loc: Point3i = PointN([loc.x(), loc.y() - 1, loc.z() - 1]);
    let bot2_loc: Point3i = PointN([loc.x(), loc.y() - 1, loc.z()]);
    let bot3_loc: Point3i = PointN([loc.x() - 1, loc.y() - 1, loc.z() - 1]);

    let top0 = if padded_chunk_extent.contains(&top0_loc) {
        let vox = padded_chunk.get(&top0_loc);
        !vox.is_empty()
    } else {
        false
    };

    let top1 = if padded_chunk_extent.contains(&top1_loc) {
        let vox = padded_chunk.get(&top1_loc);
        !vox.is_empty()
    } else {
        false
    };

    let top2 = if padded_chunk_extent.contains(&top2_loc) {
        let vox = padded_chunk.get(&top2_loc);
        !vox.is_empty()
    } else {
        false
    };

    let top3 = if padded_chunk_extent.contains(&top3_loc) {
        let vox = padded_chunk.get(&top3_loc);
        !vox.is_empty()
    } else {
        false
    };

    let bot0 = if padded_chunk_extent.contains(&bot0_loc) {
        let vox = padded_chunk.get(&bot0_loc);
        !vox.is_empty()
    } else {
        false
    };

    let bot1 = if padded_chunk_extent.contains(&bot1_loc) {
        let vox = padded_chunk.get(&bot1_loc);
        !vox.is_empty()
    } else {
        false
    };

    let bot2 = if padded_chunk_extent.contains(&bot2_loc) {
        let vox = padded_chunk.get(&bot2_loc);
        !vox.is_empty()
    } else {
        false
    };

    let bot3 = if padded_chunk_extent.contains(&bot3_loc) {
        let vox = padded_chunk.get(&bot3_loc);
        !vox.is_empty()
    } else {
        false
    };

    let (side0, side1, corner) = if !top0 && bot0 {
        (top2, top3, top1)
    } else {
        if !top1 && bot1 {
            (top2, top3, top0)
        } else {
            if !top2 && bot2 {
                (top0, top1, top3)
            } else {
                if !top3 && bot3 {
                    (top0, top1, top2)
                } else {
                    return 0;
                }
            }
        }
    };

    if side0 && side1 {
        return 3;
    } else {
        return side0 as i32 + side1 as i32 + corner as i32;
    }
}

fn get_chunk_voxels(res: &mut ResMut<GeneratedVoxelResource>, min: Point3i, max: Point3i) {
    let mut rng = rand::thread_rng();
    let random_material = rng.gen_range(1, 4);
    let yoffset = SEA_LEVEL;
    let yscale = TERRAIN_Y_SCALE * yoffset;
    for z in min.z()..max.z() {
        for x in min.x()..max.x() {
            let max_y = (res.noise.get([x as f64, z as f64]) * yscale + yoffset).round() as i32;
            for y in 0..(max_y + 1) {
                let (_p, v) = res.map.get_mut_point_and_chunk_key(&PointN([x, y, z]));
                *v = Voxel(random_material);
            }
        }
    }
}

#[derive(Bundle)]
pub struct GenerateAtTag;

struct GeneratedMeshesResource {
    pub generated_map: HashMap<Point3i, (Entity, Handle<Mesh>, RigidBodyHandle)>,
}

impl Default for GeneratedMeshesResource {
    fn default() -> Self {
        GeneratedMeshesResource {
            generated_map: HashMap::new(),
        }
    }
}

fn generate_chunks_system(
    mut voxels: ResMut<GeneratedVoxelResource>,
    voxel_meshes: Res<GeneratedMeshesResource>,
    query: Query<&Transform, With<GenerateAtTag>>,
) {
    let cam_transform = query.iter().next().expect("Failed to get camera transform");
    let cam_pos = cam_transform.translation;
    let cam_pos = PointN([cam_pos.x.round() as i32, 0i32, cam_pos.z.round() as i32]);

    let extent = transform_to_extent(cam_pos, voxels.view_distance);
    let extent = extent_modulo_expand(extent, voxels.chunk_size);
    let min = extent.minimum;
    let max = extent.least_upper_bound();

    let chunk_size = voxels.chunk_size;
    let max_height = voxels.max_height;
    let vd2 = voxels.view_distance * voxels.view_distance;
    for z in (min.z()..max.z()).step_by(voxels.chunk_size as usize) {
        for x in (min.x()..max.x()).step_by(voxels.chunk_size as usize) {
            let p = PointN([x, 0, z]);
            let d = p - cam_pos;
            if voxel_meshes.generated_map.get(&p).is_some() || d.dot(&d) > vd2 {
                continue;
            }
            get_chunk_voxels(
                &mut voxels,
                PointN([x, 0, z]),
                PointN([x + chunk_size, max_height, z + chunk_size]),
            );

            // save_chunk_to_file(
            //     p,
            //     &voxels.map,
            //     Extent3i::from_min_and_shape(p, PointN([chunk_size, max_height, chunk_size])),
            // )
            // .expect("Failed to save chunk to file");
        }
    }
}

fn modulo_down(v: i32, modulo: i32) -> i32 {
    (v / modulo) * modulo
}

fn modulo_up(v: i32, modulo: i32) -> i32 {
    ((v / modulo) + 1) * modulo
}

fn transform_to_extent(cam_pos: Point3i, view_distance: i32) -> Extent3i {
    Extent3i::from_min_and_lub(
        PointN([cam_pos.x() - view_distance, 0, cam_pos.z() - view_distance]),
        PointN([cam_pos.x() + view_distance, 0, cam_pos.z() + view_distance]),
    )
}

fn extent_modulo_expand(extent: Extent3i, modulo: i32) -> Extent3i {
    let min = extent.minimum;
    let max = extent.least_upper_bound();
    Extent3i::from_min_and_lub(
        PointN([
            modulo_down(min.x(), modulo),
            min.y(),
            modulo_down(min.z(), modulo),
        ]),
        PointN([
            modulo_up(max.x(), modulo) + 1,
            max.y() + 1,
            modulo_up(max.z(), modulo) + 1,
        ]),
    )
}

fn texture_layer_from_voxel_type(voxel_type: VoxelType) -> (i32, i32, i32) {
    match voxel_type {
        1 => (1, 2, 3),
        2 => (3, 3, 3),
        3 => (4, 4, 4),
        4 => (5, 5, 5),
        _ => (0, 0, 0),
    }
}

fn process_quad_buffer(
    buffer: GreedyQuadsBuffer<VoxelType>,
    padded_chunk: &ArrayN<[i32; 3], Voxel>,
    padded_chunk_extent: &Extent3i,
) -> Option<ChunkMeshData> {
    let mut vert_vox_mat_vals: Vec<f32> = Vec::new();
    let mut vert_ao_vals: Vec<f32> = Vec::new();
    let mut mesh = PosNormTexMesh::default();
    for group in buffer.quad_groups.iter() {
        for (quad, material) in group.quads.iter() {
            for v in group.face.quad_corners(quad).iter() {
                let v_ao = get_ao_at_vert(*v, padded_chunk, padded_chunk_extent) as f32;
                vert_ao_vals.extend_from_slice(&[v_ao]);
            }

            group.face.add_quad_to_pos_norm_tex_mesh(&quad, &mut mesh);

            let (top_tex_layer, sides_tex_layer, bottom_tex_layer) =
                texture_layer_from_voxel_type(*material);
            let voxel_mat = match group.face.n {
                PointN([0, 1, 0]) => {
                    if group.face.n_sign > 0 {
                        top_tex_layer
                    } else {
                        bottom_tex_layer
                    }
                }
                _ => sides_tex_layer,
            } as f32;

            vert_vox_mat_vals.extend_from_slice(&[voxel_mat, voxel_mat, voxel_mat, voxel_mat]);
        }
    }

    if mesh.is_empty() {
        None
    } else {
        Some(ChunkMeshData {
            pos_norm_tex_mesh: mesh,
            vert_vox_mat_vals,
            vert_ao_vals,
        })
    }
}

fn generate_mesh(voxel_map: &VoxelMap, extent: Extent3i) -> ChunkMeshData {
    let extent_padded = extent.padded(1);
    let mut map = Array3::fill(extent_padded, Voxel(0));
    copy_extent(&extent_padded, voxel_map, &mut map);
    let mut quads = GreedyQuadsBuffer::new(extent_padded);
    greedy_quads(&map, &extent_padded, &mut quads);

    process_quad_buffer(quads, &map, &extent_padded).unwrap()
}

fn create_chunk_entity(
    commands: &mut Commands,
    meshes: &mut ResMut<Assets<Mesh>>,
    mut bodies: &mut ResMut<RigidBodySet>,
    colliders: &mut ResMut<ColliderSet>,
    mesh_data: ChunkMeshData,
    voxel_material: Handle<TerrainMaterial>,
    pipelines: &RenderPipelines,
) -> (Entity, Handle<Mesh>, RigidBodyHandle) {
    let mut render_mesh = Mesh::new(PrimitiveTopology::TriangleList);

    render_mesh.set_attribute(
        Mesh::ATTRIBUTE_POSITION,
        VertexAttributeValues::Float3(mesh_data.pos_norm_tex_mesh.positions.clone()),
    );
    render_mesh.set_attribute(
        Mesh::ATTRIBUTE_NORMAL,
        VertexAttributeValues::Float3(mesh_data.pos_norm_tex_mesh.normals),
    );
    render_mesh.set_attribute(
        Mesh::ATTRIBUTE_UV_0,
        VertexAttributeValues::Float2(mesh_data.pos_norm_tex_mesh.tex_coords),
    );
    render_mesh.set_attribute(
        "Vertex_Voxel_Material",
        VertexAttributeValues::Float(mesh_data.vert_vox_mat_vals),
    );

    render_mesh.set_attribute(
        "Vertex_AO",
        VertexAttributeValues::Float(mesh_data.vert_ao_vals),
    );

    let indices: Vec<u32> = mesh_data
        .pos_norm_tex_mesh
        .indices
        .iter()
        .map(|i| *i as u32)
        .collect();

    render_mesh.set_indices(Some(Indices::U32(indices.clone())));

    let mesh = meshes.add(render_mesh);

    let vertices = mesh_data
        .pos_norm_tex_mesh
        .positions
        .iter()
        .map(|p| bevy_rapier3d::rapier::math::Point::from_slice(p))
        .collect();
    let indices = indices
        .chunks(3)
        .map(|i| bevy_rapier3d::rapier::na::Point3::<u32>::from_slice(i))
        .collect();

    let body_handle = bodies.insert(RigidBodyBuilder::new_static().build());
    let collider_handle = colliders.insert(
        ColliderBuilder::trimesh(vertices, indices).build(),
        body_handle,
        &mut bodies,
    );

    let entity = commands
        .spawn(MeshBundle {
            mesh: mesh.clone(),
            render_pipelines: pipelines.to_owned(),
            ..Default::default()
        })
        .with(voxel_material)
        .with_bundle((
            RigidBodyHandleComponent::from(body_handle),
            ColliderHandleComponent::from(collider_handle),
        ))
        .current_entity()
        .unwrap();
    (entity, mesh, body_handle)
}

fn generate_chunk_meshes_system(
    mut commands: &mut Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut bodies: ResMut<RigidBodySet>,
    mut colliders: ResMut<ColliderSet>,
    mut joints: ResMut<JointSet>,
    voxels: ChangedRes<GeneratedVoxelResource>,
    mut voxel_meshes: ResMut<GeneratedMeshesResource>,
    query: Query<&Transform, With<GenerateAtTag>>,
    assets: ResMut<VoxelAssetHandles>,
) {
    let cam_transform = query.iter().next().expect("Failed to get camera transform");
    let cam_pos = cam_transform.translation;
    let cam_pos = PointN([cam_pos.x.round() as i32, 0i32, cam_pos.z.round() as i32]);

    let pipelines =
        RenderPipelines::from_pipelines(vec![RenderPipeline::new(assets.pipeline.clone())]);

    let view_distance = voxels.view_distance;
    let chunk_size = voxels.chunk_size;
    let extent = transform_to_extent(cam_pos, view_distance);
    let extent = extent_modulo_expand(extent, chunk_size);
    let min = extent.minimum;
    let max = extent.least_upper_bound();

    let max_height = voxels.max_height;
    let vd2 = view_distance * view_distance;
    let mut to_remove: HashSet<Point3i> = voxel_meshes.generated_map.keys().cloned().collect();
    for z in (min.z()..max.z()).step_by(chunk_size as usize) {
        for x in (min.x()..max.x()).step_by(chunk_size as usize) {
            let p = PointN([x, 0, z]);
            let d = p - cam_pos;
            if d.dot(&d) > vd2 {
                continue;
            }
            to_remove.remove(&p);
            if voxel_meshes.generated_map.get(&p).is_some() {
                continue;
            }

            let builder = ChunkMapBuilder {
                chunk_shape: PointN([CHUNK_SIZE; 3]),
                ambient_value: Voxel(0),
                default_chunk_metadata: (),
            };

            let mut loaded_map = builder.build_with_hash_map_storage();

            // load_chunk_from_file(
            //     p,
            //     &mut loaded_map,
            //     Extent3i::from_min_and_shape(p, PointN([chunk_size, max_height, chunk_size])),
            // )
            // .expect("Could not load chunk from file");

            let mesh_data = generate_mesh(
                &voxels.map,
                //&loaded_map,
                Extent3i::from_min_and_shape(p, PointN([chunk_size, max_height, chunk_size])),
            );

            let entity_mesh = create_chunk_entity(
                &mut commands,
                &mut meshes,
                &mut bodies,
                &mut colliders,
                mesh_data,
                assets.material.clone(),
                &pipelines,
            );
            voxel_meshes.generated_map.insert(p, entity_mesh);
        }
    }
    for p in &to_remove {
        if let Some((entity, mesh, body)) = voxel_meshes.generated_map.remove(p) {
            commands.despawn(entity);
            meshes.remove(&mesh);
            bodies.remove(body, &mut *colliders, &mut *joints);
        }
    }
}
